//! Exercise the real app reading-column clamp, footer and native keyboard path.
//! Tasks that could launch a provider are never run; this is an unsent draft.
use super::*;
use iced::advanced::{
    clipboard,
    widget::{Operation, operation},
};
use iced::futures::{FutureExt, StreamExt};
use iced::runtime::{UserInterface, user_interface::Cache};
use iced_accessibility::accesskit::Role;

type Ui<'a> = UserInterface<'a, Message, cosmic::Theme, cosmic::Renderer>;

#[derive(Default)]
struct Probe {
    footer: Option<(widget::Id, iced::Rectangle)>,
    focused: Option<widget::Id>,
}
impl Operation for Probe {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
        if let Some((wanted, saved)) = &mut self.footer
            && id == Some(wanted)
        {
            *saved = bounds;
        }
    }
    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        _: iced::Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if state.is_focused() {
            self.focused = id.cloned();
        }
    }
}

fn enter(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer) -> Vec<Message> {
    let mut messages = Vec::new();
    ui.update(
        &[
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }),
            iced::Event::Keyboard(keyboard::Event::KeyReleased {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            }),
        ],
        iced::mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    messages
}

fn focus(ui: &mut Ui<'_>, renderer: &cosmic::Renderer, id: widget::Id) {
    ui.operate(renderer, &mut operation::focusable::focus::<()>(id.clone()));
    let mut focused = Probe::default();
    ui.operate(renderer, &mut focused);
    assert_eq!(
        focused.focused,
        Some(id),
        "the real native control owns Enter"
    );
}

fn focus_effects(ui: &mut Ui<'_>, renderer: &cosmic::Renderer, task: Task<Message>) {
    assert_eq!(
        task.units(),
        1,
        "opening the picker schedules one focus effect"
    );
    if let Some(mut effects) = iced::runtime::task::into_stream(task) {
        while let Some(effect) = effects
            .next()
            .now_or_never()
            .expect("opening the model picker only emits immediate native focus")
        {
            let iced::runtime::Action::Widget(mut operation) = effect else {
                panic!("opening the picker must not start a provider or send a message")
            };
            ui.operate(renderer, operation.as_mut());
        }
    }
}

fn group_bounds(ui: &mut Ui<'_>) -> Option<[iced::Rectangle; 4]> {
    let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
    let find = |role, label| {
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().role() == role && node.node().label() == Some(label))
            .and_then(|node| node.node().bounds())
            .map(|rect| iced::Rectangle {
                x: rect.x0 as f32,
                y: rect.y0 as f32,
                width: (rect.x1 - rect.x0) as f32,
                height: (rect.y1 - rect.y0) as f32,
            })
    };
    Some([
        find(Role::Button, "Choose a model")?,
        find(Role::Button, "Choose reasoning effort")?,
        find(Role::Paragraph, "50%")?,
        find(Role::Paragraph, "$1.25")?,
    ])
}

#[test]
fn real_chat_footer_keeps_right_group_visible_and_narrow_model_selection_keyboard_reachable() {
    let mut renderer = crate::app::tests::headless_renderer();
    for width in [260.0, 620.0, 640.0, 720.0, 1200.0] {
        let directory = tempfile::Builder::new()
            .prefix("cagents-footer-with-long-project-context-")
            .tempdir()
            .unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.settings.default_native_pane_fraction = 1.0;
        drop(app.new_native_chat(Provider::Codex, Some(directory.path().into())));
        let card = app.workspace.focused().unwrap();
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(width),
        }));
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("a real new chat draft")
        };
        assert!(runtime.is_draft());
        assert!(runtime.visible.load(Ordering::Relaxed));
        let target = runtime.target;
        runtime.replace_draft(text_editor::Content::with_text("Keep this 🦀 unsent draft"));
        runtime
            .attachments
            .push(Attachment::file(directory.path().join("reference.txt")));
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.capabilities.model_selection = true;
        snapshot.capabilities.reasoning_selection = true;
        snapshot.models = vec![chat::Model {
            id: "fixture-model".into(),
            label: "A deliberately long model label".into(),
            reasoning: vec!["low".into(), "high".into()],
        }];
        snapshot.selected_model = Some("fixture-model".into());
        snapshot.reasoning = Some("high".into());
        snapshot.usage.context_limit = 200_000;
        snapshot.usage.context_tokens = 100_000;
        snapshot.usage.cost_microusd = Some(1_250_000);
        let before = (
            runtime.composer.text(),
            runtime.composer.cursor(),
            runtime.attachments.clone(),
            runtime.next_command,
        );
        let viewport = iced::Size::new(width, 760.0);
        let mut ui =
            UserInterface::build(app.chat_view(card), viewport, Cache::new(), &mut renderer);
        let mut footer = Probe {
            footer: Some((
                card_control_id(card, "chat-footer"),
                iced::Rectangle::default(),
            )),
            ..Default::default()
        };
        ui.operate(&renderer, &mut footer);
        let footer = footer.footer.unwrap().1;
        assert!(footer.width > 0.0 && footer.width <= width);
        assert!(footer.width <= design::READABLE_MEASURE + 2.0 * step::GAP_M);
        assert!(
            (footer.width - width.min(design::READABLE_MEASURE + 2.0 * step::GAP_M)).abs() <= 1.0,
            "the real chat_view uses the capped reading width: {width}: {footer:?}"
        );
        if width >= 640.0 {
            let group = group_bounds(&mut ui)
                .expect("model, effort, usage and cost are visible in the real app footer");
            for bounds in group {
                assert!(bounds.width > 0.0 && bounds.height > 0.0);
                assert!(
                    bounds.x >= footer.x
                        && bounds.x + bounds.width <= footer.x + footer.width + 1.0,
                    "{width}: {bounds:?} outside {footer:?}"
                );
                assert!(
                    bounds.y >= footer.y
                        && bounds.y + bounds.height <= footer.y + footer.height + 1.0
                );
            }
            for pair in group.windows(2) {
                assert!(
                    pair[0].x + pair[0].width <= pair[1].x,
                    "{width}: right-group controls overlap: {pair:?}"
                );
            }
            assert!(
                group[0].x > footer.x + footer.width / 3.0,
                "context and model group have flexible separation"
            );
            assert!(
                (group[3].x + group[3].width
                    - (footer.x + footer.width - step::GAP_M - COMPOSER_PADDING))
                    .abs()
                    <= 1.0,
                "cost anchors the right edge"
            );
        } else {
            assert!(
                group_bounds(&mut ui).is_none(),
                "narrow details are initially folded"
            );
            focus(
                &mut ui,
                &renderer,
                card_control_id(card, "chat-footer-details"),
            );
            let messages = enter(&mut ui, &mut renderer);
            assert!(
                matches!(messages.as_slice(), [Message::ChatUi(Action::FooterPicker(actual, true))] if *actual == target)
            );
            let cache = ui.into_cache();
            drop(app.handle_chat_ui(Action::FooterPicker(target, true)));
            ui = UserInterface::build(app.chat_view(card), viewport, cache, &mut renderer);
            // Operate lays out the actual popup before AX inspects its subtree.
            focus(
                &mut ui,
                &renderer,
                card_control_id(card, "chat-model-picker"),
            );
            let group =
                group_bounds(&mut ui).expect("More exposes all four details through native AX");
            for bounds in group {
                assert!(bounds.width > 0.0 && bounds.height > 0.0);
                assert!(
                    bounds.x >= 0.0 && bounds.x + bounds.width <= width + 1.0,
                    "{width}: wrapped detail remains in the private viewport: {bounds:?}"
                );
                assert!(bounds.y >= 0.0 && bounds.y + bounds.height <= viewport.height);
            }
            for pair in group.windows(2) {
                assert!(
                    pair[0].y + pair[0].height <= pair[1].y
                        || pair[0].x + pair[0].width <= pair[1].x,
                    "{width}: wrapped details must not overlap: {pair:?}"
                );
            }
        }
        focus(
            &mut ui,
            &renderer,
            card_control_id(card, "chat-model-picker"),
        );
        let messages = enter(&mut ui, &mut renderer);
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::ModelPicker(actual, true))] if *actual == target)
        );
        let cache = ui.into_cache();
        let task = app.handle_chat_ui(Action::ModelPicker(target, true));
        let mut ui = UserInterface::build(app.chat_view(card), viewport, cache, &mut renderer);
        focus_effects(&mut ui, &renderer, task);
        let mut focused = Probe::default();
        ui.operate(&renderer, &mut focused);
        assert_eq!(
            focused.focused,
            Some(card_control_id(card, "chat-model-search"))
        );
        let mut choices = enter(&mut ui, &mut renderer);
        assert!(
            matches!(choices.as_slice(), [Message::ChatUi(Action::DraftModel(actual, Provider::Codex, None))] if *actual == target),
            "native search Enter reaches the provider's real default-model choice: {choices:?}"
        );
        drop(ui);
        let Message::ChatUi(choice) = choices.pop().unwrap() else {
            unreachable!()
        };
        drop(app.handle_chat_ui(choice));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert!(runtime.snapshot.selected_model.is_none());
        assert_eq!(
            (
                runtime.composer.text(),
                runtime.composer.cursor(),
                runtime.attachments.clone(),
                runtime.next_command
            ),
            before
        );
        assert!(runtime.pending.is_none() && runtime.queued.is_none());
    }
}
