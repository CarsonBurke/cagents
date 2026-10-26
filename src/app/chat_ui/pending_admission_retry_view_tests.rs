//! Child of the owning startup fixture: real finite full/closed admission
//! failures, actual app.chat_view widgets, no daemon or polled startup task.
use super::*;
use crate::app::workspace_model_sync::ModelUpdate;
use crate::chat::{Attachment, ImageReference};
use cosmic::Application as _;
use cosmic::iced::{
    Size,
    advanced::{clipboard, mouse},
    runtime::user_interface::{Cache, UserInterface},
};

type Retry = crate::app::shared_views::SharedChatAdmissionRetry;

fn attachment(number: u64, name: &str) -> Attachment {
    Attachment {
        path: format!("/never-open-saved-attachment/{number}.png").into(),
        reference: Some(ImageReference {
            id: uuid::Uuid::new_v4(),
            number,
            device: uuid::Uuid::new_v4(),
            name: name.into(),
            sha256: "a".repeat(64),
            extension: "png".into(),
        }),
    }
}

fn saved(
    text: &str,
    images: Vec<Attachment>,
    closed: bool,
) -> (Cagents, CardId, async_channel::Receiver<ModelEdit>) {
    let (mut app, active, hidden, pane, identity) = cached_chat();
    app.chat_ui = chat_ui::State::with_draft_attachments(
        HashMap::from([(pane.id, text.to_owned())]),
        HashMap::from([(pane.id, images)]),
    );
    let (tx, rx) = async_channel::bounded(1);
    tx.try_send(ModelEdit::SeedView {
        scope: Scope::Channel(active),
        panes: vec![],
        closed: vec![],
    })
    .unwrap();
    if closed {
        rx.close();
    }
    app.workspace_model_tx = tx;
    // No task is polled. AwaitingAdmission installs only the local saved view.
    drop(app.initialize_runtimes());
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    drop(
        app.apply_workspace_model_update(ModelUpdate::Connected(model(
            identity, active, hidden, view,
        ))),
    );
    assert!(app.chat_awaiting_admission(pane.id));
    let failure = app
        .shared_chat_admission_failure(pane.id)
        .expect("real try_send refusal");
    assert!(
        failure
            .message()
            .contains(if closed { "stopped" } else { "full" })
    );
    assert_eq!(failure.retry().is_none(), closed);
    assert_eq!(
        rx.len(),
        1,
        "fixture queue remains bounded, no provider request polled"
    );
    (app, pane.id, rx)
}

fn retry(app: &Cagents, card: CardId) -> Retry {
    app.shared_chat_admission_failure(card)
        .unwrap()
        .retry()
        .unwrap()
}

fn assert_retry(messages: &[Message], expected: &Retry) {
    assert!(
        matches!(messages, [Message::RetrySharedChatAdmission(actual)] if actual == expected),
        "exact native retry action required: {messages:?}"
    );
}

fn preserved(
    app: &mut Cagents,
    card: CardId,
    original: &Arc<WorkspaceState>,
    queue: &async_channel::Receiver<ModelEdit>,
) {
    assert!(app.chat_awaiting_admission(card));
    assert_eq!(app.persistent_workspace().as_ref(), original.as_ref());
    assert_eq!(
        queue.len(),
        1,
        "rendering/callbacks never grow the full publisher queue"
    );
}

type Ui<'a> = UserInterface<'a, Message, cosmic::Theme, cosmic::Renderer>;

fn mount<'a>(
    app: &'a Cagents,
    card: CardId,
    width: f32,
    cache: Cache,
    renderer: &mut cosmic::Renderer,
) -> Ui<'a> {
    let mut ui = UserInterface::build(
        app.chat_view(card),
        Size::new(width, 900.0),
        cache,
        renderer,
    );
    ui.draw(
        renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
    ui
}

fn labels(ui: &Ui<'_>) -> Vec<String> {
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .flat_map(|node| [node.node().label(), node.node().value()])
        .flatten()
        .map(str::to_owned)
        .collect()
}

fn retry_bounds(ui: &Ui<'_>, width: f32) -> iced::Rectangle {
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let retry = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter(|node| {
            node.node().role() == iced_accessibility::accesskit::Role::Button
                && node.node().label() == Some("Retry workspace connection")
        })
        .collect::<Vec<_>>();
    assert_eq!(retry.len(), 1, "exactly one accessible retry");
    assert!(!retry[0].node().is_disabled() && !retry[0].node().is_hidden());
    let rect = retry[0].node().bounds().expect("native retry bounds");
    assert!(rect.x0 >= 0.0 && rect.x1 <= f64::from(width));
    assert!(rect.y0 >= 0.0 && rect.y1 <= 900.0);
    assert!(rect.x1 > rect.x0 && rect.y1 > rect.y0);
    iced::Rectangle {
        x: rect.x0 as f32,
        y: rect.y0 as f32,
        width: (rect.x1 - rect.x0) as f32,
        height: (rect.y1 - rect.y0) as f32,
    }
}

fn event(
    ui: &mut Ui<'_>,
    renderer: &mut cosmic::Renderer,
    event: iced::Event,
    cursor: mouse::Cursor,
) -> Vec<Message> {
    let mut messages = Vec::new();
    let _ = ui.update(
        &[event],
        cursor,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    messages
}

fn pointer(
    ui: &mut Ui<'_>,
    renderer: &mut cosmic::Renderer,
    point: iced::Point,
    down: bool,
) -> Vec<Message> {
    event(
        ui,
        renderer,
        iced::Event::Mouse(if down {
            mouse::Event::ButtonPressed(mouse::Button::Left)
        } else {
            mouse::Event::ButtonReleased(mouse::Button::Left)
        }),
        mouse::Cursor::Available(point),
    )
}

fn enter(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer) -> Vec<Message> {
    use iced::keyboard::{self, key};
    event(
        ui,
        renderer,
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key::Named::Enter),
            modified_key: keyboard::Key::Named(key::Named::Enter),
            physical_key: key::Physical::Code(key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }),
        mouse::Cursor::Unavailable,
    )
}

fn focus_retry(ui: &mut Ui<'_>, renderer: &cosmic::Renderer, card: CardId) {
    use iced::advanced::widget::operation;
    let id = card_control_id(card, "saved.admission.retry");
    ui.operate(renderer, &mut operation::focusable::focus::<()>(id.clone()));
    struct Focus {
        id: widget::Id,
        focused: bool,
    }
    impl iced::advanced::widget::Operation for Focus {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if id == Some(&self.id) {
                self.focused = state.is_focused();
            }
        }
    }
    let mut probe = Focus { id, focused: false };
    ui.operate(renderer, &mut probe);
    assert!(
        probe.focused,
        "actual retry native focus must exist before Enter"
    );
}

#[test]
fn saved_empty_text_and_image_only_views_remain_honest_and_preserve_receipts_at_all_widths() {
    for (text, images) in [
        ("", vec![]),
        ("  Exact 🦀 [Image 41]\ntrailing space \n", vec![]),
        (
            "",
            vec![
                attachment(41, "Original diagram.png"),
                attachment(7, "Second original.png"),
            ],
        ),
    ] {
        let (mut app, card, queue) = saved(text, images.clone(), false);
        let original = app.persistent_workspace();
        let expected_text = match app.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime.unsent_text(),
            _ => panic!("saved runtime"),
        };
        let mut renderer = crate::app::tests::headless_renderer();
        for width in [280.0, 640.0, 1000.0] {
            let mut ui = mount(&app, card, width, Cache::default(), &mut renderer);
            let shown = labels(&ui);
            for label in ["Saved conversation", "Not connected"] {
                assert!(shown.iter().any(|text| text == label), "{shown:?}");
            }
            assert!(
                !shown
                    .iter()
                    .any(|label| label.contains("while the workspace reconnects")
                        || label.contains("Connecting…")
                        || label.contains("Loading…")
                        || label.contains("open the conversation from history"))
            );
            assert_eq!(
                shown.iter().any(|label| label == "No unsent draft."),
                text.is_empty() && images.is_empty()
            );
            assert_eq!(
                shown.iter().any(|label| label == "Unsent draft"),
                !text.is_empty() || !images.is_empty()
            );
            let _ = retry_bounds(&ui, width);
            let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
            let buttons = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
                .collect::<Vec<_>>();
            assert_eq!(buttons.len(), 1 + usize::from(!expected_text.is_empty()));
            if !expected_text.is_empty() {
                use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, TreeId};
                let copy = buttons
                    .iter()
                    .find(|node| node.node().label() == Some("Copy saved draft"))
                    .unwrap();
                let target_node = NodeId::from(copy.id().clone());
                let messages = event(
                    &mut ui,
                    &mut renderer,
                    iced::Event::A11y(
                        iced::advanced::widget::Id::from(u128::from(target_node.0) as u64),
                        ActionRequest {
                            action: Action::Click,
                            target_tree: TreeId::ROOT,
                            target_node,
                            data: None,
                        },
                    ),
                    mouse::Cursor::Unavailable,
                );
                assert!(
                    matches!(messages.as_slice(), [Message::ChatUi(chat_ui::Action::CopyLiteral(actual))] if actual == &expected_text)
                );
            }
            for image in &images {
                let reference = image.reference.as_ref().unwrap();
                assert!(
                    shown.iter().any(
                        |label| label == &format!("{} · {}", reference.label(), reference.name)
                    )
                );
            }
            assert!(
                !shown
                    .iter()
                    .any(|label| label.contains("never-open-saved-attachment"))
            );
            drop(ui);
            preserved(&mut app, card, &original, &queue);
        }
        assert_eq!(
            original
                .chat_draft_attachments
                .get(&card)
                .cloned()
                .unwrap_or_default(),
            images
        );
    }
}

#[test]
fn closed_publisher_exposes_this_window_instruction_without_retry_or_keyboard_action() {
    let (mut app, card, queue) = saved("", vec![], true);
    let original = app.persistent_workspace();
    let message = app
        .shared_chat_admission_failure(card)
        .unwrap()
        .message()
        .to_owned();
    assert!(message.contains("close and reopen this window"));
    let mut renderer = crate::app::tests::headless_renderer();
    for width in [280.0, 1000.0] {
        let mut ui = mount(&app, card, width, Cache::default(), &mut renderer);
        assert!(labels(&ui).contains(&message));
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        assert!(
            !nodes
                .root()
                .iter()
                .chain(nodes.children())
                .any(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        );
        ui.operate(
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                card,
                "saved.admission.retry",
            )),
        );
        assert!(enter(&mut ui, &mut renderer).is_empty());
        drop(ui);
        preserved(&mut app, card, &original, &queue);
    }
}

#[test]
fn native_retry_keyboard_dispatches_exact_attempt_and_queued_old_action_cannot_retry_its_successor()
{
    let (mut app, card, queue) = saved(
        "Unsent [Image 41]\n",
        vec![attachment(41, "Keep receipt.png")],
        false,
    );
    let original = app.persistent_workspace();
    let a = retry(&app, card);
    let mut renderer = crate::app::tests::headless_renderer();
    let mut ui = mount(&app, card, 640.0, Cache::default(), &mut renderer);
    focus_retry(&mut ui, &renderer, card);
    let mut messages = enter(&mut ui, &mut renderer);
    assert_retry(&messages, &a);
    let action_a = messages.pop().unwrap();
    let cache = ui.into_cache();
    drop(app.update(action_a.clone()));
    let b = retry(&app, card);
    assert_ne!(
        a, b,
        "actual full publisher retry becomes a new finite attempt"
    );
    drop(app.update(action_a));
    assert_eq!(retry(&app, card), b, "queued A must not retry B");
    preserved(&mut app, card, &original, &queue);
    let mut ui = mount(&app, card, 640.0, cache, &mut renderer);
    focus_retry(&mut ui, &renderer, card);
    let mut messages = enter(&mut ui, &mut renderer);
    assert_retry(&messages, &b);
    drop(ui);
    drop(app.update(messages.pop().unwrap()));
    assert_ne!(retry(&app, card), b, "fresh exact B action remains usable");
    preserved(&mut app, card, &original, &queue);
}

#[test]
fn retained_retry_press_cannot_cross_attempts_and_unchanged_attempt_keeps_its_native_press() {
    for replace in [false, true] {
        let (mut app, card, queue) =
            saved("Keep draft", vec![attachment(9, "Keep image.png")], false);
        let original = app.persistent_workspace();
        let a = retry(&app, card);
        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui = mount(&app, card, 640.0, Cache::default(), &mut renderer);
        // Capture the real callback before holding the pointer. No intervening
        // widget event can clear or alter the held press before replacement.
        let advance = if replace {
            focus_retry(&mut ui, &renderer, card);
            let mut messages = enter(&mut ui, &mut renderer);
            assert_retry(&messages, &a);
            Some(messages.pop().unwrap())
        } else {
            None
        };
        let hit = retry_bounds(&ui, 640.0).center();
        assert!(pointer(&mut ui, &mut renderer, hit, true).is_empty());
        let cache = ui.into_cache();
        if let Some(action) = advance {
            drop(app.update(action));
        }
        let current = retry(&app, card);
        assert_eq!(current == a, !replace);
        let mut ui = mount(&app, card, 640.0, cache, &mut renderer);
        assert_eq!(
            retry_bounds(&ui, 640.0).center(),
            hit,
            "same native hit area isolates ownership"
        );
        let released = pointer(&mut ui, &mut renderer, hit, false);
        if replace {
            assert!(
                released.is_empty(),
                "held A must not activate B: {released:?}"
            );
        } else {
            assert_retry(&released, &a);
        }
        assert!(pointer(&mut ui, &mut renderer, hit, true).is_empty());
        assert_retry(&pointer(&mut ui, &mut renderer, hit, false), &current);
        drop(ui);
        preserved(&mut app, card, &original, &queue);
    }
}

#[test]
fn disabled_retry_keeps_failure_visible_and_reenabled_retry_requires_a_fresh_press() {
    let (mut app, card, queue) = saved("Keep original", vec![], false);
    let original = app.persistent_workspace();
    let token = retry(&app, card);
    let failure = app
        .shared_chat_admission_failure(card)
        .unwrap()
        .message()
        .to_owned();
    let mut renderer = crate::app::tests::headless_renderer();
    let mut ui = mount(&app, card, 640.0, Cache::default(), &mut renderer);
    let hit = retry_bounds(&ui, 640.0).center();
    assert!(pointer(&mut ui, &mut renderer, hit, true).is_empty());
    let cache = ui.into_cache();
    app.update_prepared = true;
    let mut ui = mount(&app, card, 640.0, cache, &mut renderer);
    assert!(labels(&ui).contains(&failure));
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let disabled = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| {
            node.node().label() == Some("Retry workspace connection")
                && node.node().role() == iced_accessibility::accesskit::Role::Button
        })
        .unwrap();
    assert!(disabled.node().is_disabled());
    assert!(pointer(&mut ui, &mut renderer, hit, false).is_empty());
    assert!(pointer(&mut ui, &mut renderer, hit, true).is_empty());
    assert!(enter(&mut ui, &mut renderer).is_empty());
    let cache = ui.into_cache();
    app.update_prepared = false;
    let mut ui = mount(&app, card, 640.0, cache, &mut renderer);
    assert_eq!(retry_bounds(&ui, 640.0).center(), hit);
    assert!(
        pointer(&mut ui, &mut renderer, hit, false).is_empty(),
        "reenabling must not inherit either held press"
    );
    assert!(pointer(&mut ui, &mut renderer, hit, true).is_empty());
    assert_retry(&pointer(&mut ui, &mut renderer, hit, false), &token);
    drop(ui);
    assert_eq!(retry(&app, card), token);
    preserved(&mut app, card, &original, &queue);
}
