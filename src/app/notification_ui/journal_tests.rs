//! Actual Attention widgets/callbacks; tasks that could contact a daemon are
//! deliberately left unpolled. Typed read completion is not an RPC claim.
use super::*;
use cosmic::iced::{
    Size,
    advanced::{clipboard, mouse},
    runtime::user_interface::{Cache, UserInterface},
};

fn automation(index: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(index + 1001)
}

fn notice(index: u128) -> crate::notifications::Notice {
    let id = uuid::Uuid::from_u128(index + 1);
    crate::notifications::Notice {
        id,
        delivered: false, // The journal must work without a desktop portal.
        acknowledged: false,
        window: None,
        event: crate::notifications::Event {
            target: Target::Automation {
                id: automation(index),
                sequence: 42,
            },
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            content: Some(crate::notifications::Content {
                conversation: "PRIVATE_CONVERSATION_SENTINEL".into(),
                preview: "PRIVATE_PREVIEW_SENTINEL".into(),
                request: None,
            }),
            evidence: crate::notifications::Evidence {
                key: format!("automation:{}:lifecycle:42", automation(index)),
                kind: crate::notifications::Kind::AutomationPaused,
            },
        },
    }
}

fn fixture(count: usize) -> Cagents {
    let mut app = super::super::tests::context_inspector_test_app(false);
    app.context_inspector_open = false;
    app.settings.notifications.message_previews = false;
    app.workspace_model.identity = uuid::Uuid::new_v4();
    drop(
        app.update_notifications(Action::HostBound(HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("attention-journal"),
            boot_id: crate::model::BootId::new("attention-journal"),
            host_incarnation: crate::model::HostIncarnation::new("attention-journal"),
        })),
    );
    snapshot(&mut app, (0..count).map(|n| notice(n as u128)).collect());
    app
}

fn snapshot(app: &mut Cagents, notices: Vec<crate::notifications::Notice>) {
    drop(app.update_notifications(Action::Snapshot(Snapshot {
        notices,
        diagnostic: None,
    })));
}

fn open(app: &mut Cagents) {
    if !app.attention_is_open() {
        drop(app.update_chrome(super::super::chrome_ui::Action::ToggleAttention));
    }
    assert!(app.attention_is_open());
}

fn label(app: &Cagents, id: uuid::Uuid) -> String {
    let position = app
        .attention_notices()
        .iter()
        .position(|notice| notice.id == id)
        .expect("current notice")
        + 1;
    format!("Open history — Automation pause recorded — entry {position}")
}

/// Build/draw the real UI before dispatching a real accessibility Click into
/// its retained widget state. No manufactured OpenNotice is used for positives.
fn frame(
    app: &Cagents,
    renderer: &mut cosmic::Renderer,
    width: f32,
    whole_window: bool,
    click: Option<&str>,
) -> (iced_accessibility::A11yTree, Vec<Message>) {
    frame_at_height(app, renderer, width, 900.0, whole_window, click)
}

fn frame_at_height(
    app: &Cagents,
    renderer: &mut cosmic::Renderer,
    width: f32,
    height: f32,
    whole_window: bool,
    click: Option<&str>,
) -> (iced_accessibility::A11yTree, Vec<Message>) {
    let element = if whole_window {
        cosmic::ApplicationExt::view_main(app)
    } else {
        app.attention_journal().map(cosmic::Action::App)
    };
    let mut ui = UserInterface::build(
        element,
        Size::new(width, height),
        Cache::default(),
        renderer,
    );
    let mut messages = Vec::new();
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(messages.is_empty());
    ui.draw(
        renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    if let Some(label) = click {
        let buttons = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some(label)
                    && !node.node().is_disabled()
                    && !node.node().is_hidden()
            })
            .collect::<Vec<_>>();
        assert_eq!(buttons.len(), 1, "one actual current button: {label}");
        let node = buttons[0];
        let bounds = node.node().bounds().expect("actual button geometry");
        assert!(bounds.x0 >= 0.0 && bounds.x1 <= f64::from(width));
        assert!(
            bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0,
            "actual enabled {label} at {width}x{height}: {bounds:?}"
        );
        assert!(bounds.y0 >= 0.0 && bounds.y1 <= f64::from(height));
        use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, TreeId};
        let target_node = NodeId::from(node.id().clone());
        let (_, statuses) = ui.update(
            &[iced::Event::A11y(
                iced::advanced::widget::Id::from(u128::from(target_node.0) as u64),
                ActionRequest {
                    action: Action::Click,
                    target_tree: TreeId::ROOT,
                    target_node,
                    data: None,
                },
            )],
            mouse::Cursor::Unavailable,
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, vec![iced::event::Status::Captured]);
    }
    let messages = messages
        .into_iter()
        .map(|message| match message {
            cosmic::Action::App(message) => message,
            _ => panic!("unexpected non-app effect from Attention click"),
        })
        .collect();
    (nodes, messages)
}

fn journal_height(app: &Cagents, renderer: &cosmic::Renderer, width: f32, height: f32) -> f32 {
    let mut element = app.attention_journal();
    let mut tree = iced::advanced::widget::Tree::new(element.as_widget());
    element
        .as_widget_mut()
        .layout(
            &mut tree,
            renderer,
            &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(width, height)),
        )
        .size()
        .height
}

fn labels(nodes: &iced_accessibility::A11yTree) -> Vec<String> {
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| node.node().label().map(str::to_owned))
        .collect()
}

fn click(app: &Cagents, renderer: &mut cosmic::Renderer, id: uuid::Uuid) -> Message {
    let (_, mut messages) = frame(app, renderer, 420.0, false, Some(&label(app, id)));
    assert_eq!(messages.len(), 1);
    let message = messages.pop().unwrap();
    assert!(matches!(
        &message,
        Message::NotificationUi(Action::OpenNotice(_))
    ));
    message
}

fn reduce(app: &mut Cagents, message: Message) {
    let Message::NotificationUi(action) = message else {
        panic!("actual notification callback");
    };
    drop(app.update_notifications(action));
}

fn no_ack(app: &Cagents, id: uuid::Uuid) {
    assert!(!app.notification_ui.acknowledged.contains(&id));
    assert!(
        !app.notification_ui
            .pending
            .lock()
            .unwrap()
            .acknowledged
            .contains(&id)
    );
    assert!(
        !app.notification_ui
            .snapshot
            .notices
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .acknowledged
    );
}

fn page(id: uuid::Uuid, sequence: u64) -> crate::automations::LifecyclePage {
    crate::automations::LifecyclePage {
        events: vec![crate::automations::LifecycleEvent {
            automation: id,
            sequence,
            revision: 3,
            occurred_ms: 600,
            cause: crate::automations::LifecycleCause::ProjectClosed,
            actor: crate::automations::LifecycleActor::System,
            enabled: false,
            blocked: false,
            firing: None,
        }],
        next: None,
        earlier_events_unavailable: false,
    }
}

#[test]
fn empty_window_attention_bell_and_narrow_journal_are_real_and_private_without_portal() {
    let mut renderer = super::super::tests::headless_renderer();
    let mut app = fixture(0);
    assert!(app.sidebar_model.projects.is_empty() && app.durable.cards.is_empty());
    let (_, mut messages) = frame(&app, &mut renderer, 1280.0, true, Some("Attention"));
    assert_eq!(messages.len(), 1);
    let Message::ChromeUi(action) = messages.pop().unwrap() else {
        panic!("real bell callback")
    };
    drop(app.update_chrome(action));
    assert!(app.attention_is_open());
    for (width, height) in [(280.0, 220.0), (420.0, 900.0)] {
        app.window_width = width;
        assert!(
            journal_height(&app, &renderer, width, height) < height.min(408.0),
            "empty journal keeps its natural compact height"
        );
        let (nodes, _) = frame_at_height(&app, &mut renderer, width, height, false, None);
        assert!(
            labels(&nodes)
                .iter()
                .any(|text| text.contains("No attention entries"))
        );
        snapshot(&mut app, vec![notice(0)]);
        let one_entry_height = journal_height(&app, &renderer, width, height);
        assert!(one_entry_height <= height.min(408.0));
        if height > 408.0 {
            assert!(
                one_entry_height < 408.0,
                "one short entry keeps its natural compact height with room available"
            );
        }
        let (nodes, messages) = frame_at_height(
            &app,
            &mut renderer,
            width,
            height,
            false,
            Some(&label(&app, notice(0).id)),
        );
        let text = labels(&nodes).join("\n");
        assert!(text.contains("Automation pause recorded") && text.contains("Open history"));
        assert!(
            !text.contains("PRIVATE_CONVERSATION_SENTINEL")
                && !text.contains("PRIVATE_PREVIEW_SENTINEL")
        );
        assert_eq!(messages.len(), 1);
        assert!(
            app.notification_ui.lifecycle_read.is_none(),
            "rendering/capturing is not activation"
        );
        no_ack(&app, notice(0).id);
        snapshot(&mut app, Vec::new());
    }
}

#[test]
fn snapshots_deduplicate_stable_rows_and_actual_more_previous_buttons_page_twenty() {
    for (width, height) in [(280.0, 220.0), (420.0, 900.0)] {
        exercise_pagination_geometry(width, height);
    }
}

fn exercise_pagination_geometry(width: f32, height: f32) {
    let mut renderer = super::super::tests::headless_renderer();
    let mut app = fixture(23);
    app.window_width = width;
    let original = app.notification_ui.snapshot.clone();
    let mut duplicate = original.clone();
    duplicate.notices.push(duplicate.notices[0].clone());
    drop(app.update_notifications(Action::Snapshot(duplicate.clone())));
    drop(app.update_notifications(Action::Snapshot(duplicate)));
    open(&mut app);
    let (nodes, messages) = frame_at_height(
        &app,
        &mut renderer,
        width,
        height,
        false,
        Some("More entries"),
    );
    assert_eq!(
        labels(&nodes)
            .iter()
            .filter(|text| text.starts_with("Open history — Automation pause recorded — entry "))
            .count(),
        20
    );
    assert_eq!(messages.len(), 1);
    let Message::ChromeUi(action) = messages.into_iter().next().unwrap() else {
        panic!("actual page callback")
    };
    drop(app.update_chrome(action));
    let (nodes, messages) = frame_at_height(
        &app,
        &mut renderer,
        width,
        height,
        false,
        Some("Previous entries"),
    );
    let row_labels = labels(&nodes);
    assert_eq!(
        row_labels
            .iter()
            .filter(|text| text.starts_with("Open history — Automation pause recorded — entry "))
            .count(),
        3
    );
    assert!(
        row_labels.contains(&label(&app, notice(22).id))
            && !row_labels.contains(&label(&app, notice(0).id))
    );
    let Message::ChromeUi(action) = messages.into_iter().next().unwrap() else {
        panic!("actual previous callback")
    };
    drop(app.update_chrome(action));
    let (nodes, _) = frame_at_height(&app, &mut renderer, width, height, false, None);
    assert!(labels(&nodes).contains(&label(&app, notice(0).id)));
    for n in &original.notices {
        no_ack(&app, n.id);
    }
    let mut conflicts = original.notices.clone();
    let mut changed = conflicts[0].clone();
    changed.event.target = Target::Automation {
        id: uuid::Uuid::new_v4(),
        sequence: 99,
    };
    conflicts.push(changed);
    snapshot(&mut app, conflicts);
    let (nodes, _) = frame_at_height(&app, &mut renderer, width, height, false, None);
    assert!(app.attention_notices().iter().all(|n| n.id != notice(0).id));
    assert_eq!(
        labels(&nodes)
            .iter()
            .filter(|text| text.starts_with("Open history —"))
            .count(),
        20
    );
    let current = click(&app, &mut renderer, notice(1).id);
    let Message::NotificationUi(Action::OpenNotice(current)) = current else {
        unreachable!()
    };
    assert_eq!(
        current.notice,
        notice(1).id,
        "conflicting ID never supplies a row callback"
    );
    snapshot(&mut app, Vec::new());
    let (nodes, _) = frame_at_height(&app, &mut renderer, width, height, false, None);
    assert!(
        labels(&nodes)
            .iter()
            .any(|text| text.contains("No attention entries"))
    );
    assert!(
        app.notification_ui
            .pending
            .lock()
            .unwrap()
            .acknowledged
            .is_empty()
    );
}

#[test]
fn rendered_automation_click_reads_exact_receipt_without_ack_or_actor_even_after_definition_absence()
 {
    let mut renderer = super::super::tests::headless_renderer();
    for available in [true, false] {
        let mut app = fixture(1);
        open(&mut app);
        let original = app.durable.cards.clone();
        let message = click(&app, &mut renderer, notice(0).id);
        reduce(&mut app, message);
        assert!(!app.attention_is_open());
        assert_eq!(
            app.durable.cards, original,
            "read must precede mounting a utility"
        );
        let activation = app.notification_ui.lifecycle_activation.as_ref().unwrap();
        assert_eq!((activation.id, activation.sequence), (automation(0), 42));
        let generation = activation.generation;
        no_ack(&app, notice(0).id);
        drop(app.update_notifications(Action::LifecycleResolved {
            generation,
            result: if available {
                Ok(page(automation(0), 42))
            } else {
                Err(())
            },
        }));
        assert!(
            app.durable
                .cards
                .iter()
                .any(|card| card.kind == CardKind::Automations)
        );
        assert!(
            app.durable
                .cards
                .iter()
                .all(|card| !matches!(card.kind, CardKind::Terminal(_) | CardKind::Chat(_)))
        );
        assert!(app.notification_ui.lifecycle_read.is_none());
        no_ack(&app, notice(0).id);
        open(&mut app);
        let (nodes, _) = frame(&app, &mut renderer, 420.0, false, None);
        assert!(
            labels(&nodes).contains(&label(&app, notice(0).id)),
            "read receipt never consumes original attention"
        );
    }
}

#[test]
fn queued_rendered_row_refuses_changed_identity_target_host_backend_registry_or_workspace() {
    let mut renderer = super::super::tests::headless_renderer();
    for changed in [
        "empty",
        "id",
        "target",
        "acknowledged",
        "conflict",
        "host-rebind",
        "new-host",
        "registry",
        "workspace",
        "backend",
        "closed",
    ] {
        let mut app = fixture(1);
        open(&mut app);
        let mut message = click(&app, &mut renderer, notice(0).id);
        match changed {
            "empty" => snapshot(&mut app, Vec::new()),
            "id" | "target" | "acknowledged" | "conflict" => {
                let mut notices = app.notification_ui.snapshot.notices.clone();
                match changed {
                    "id" => notices[0].id = uuid::Uuid::new_v4(),
                    "target" => {
                        notices[0].event.target = Target::Automation {
                            id: automation(0),
                            sequence: 43,
                        }
                    }
                    "acknowledged" => notices[0].acknowledged = true,
                    "conflict" => {
                        let mut n = notices[0].clone();
                        n.event.target = Target::Automation {
                            id: automation(0),
                            sequence: 43,
                        };
                        notices.push(n);
                    }
                    _ => unreachable!(),
                }
                snapshot(&mut app, notices);
            }
            "host-rebind" | "new-host" => {
                let mut host = app.notification_ui.host.clone().unwrap();
                if changed == "new-host" {
                    host.host_incarnation = crate::model::HostIncarnation::new("replacement");
                }
                drop(app.update_notifications(Action::HostBound(host)));
                let (nodes, _) = frame(&app, &mut renderer, 420.0, false, None);
                let text = labels(&nodes);
                assert!(text.iter().any(|value| value.contains("not connected")));
                assert!(
                    text.iter()
                        .all(|value| !value.starts_with("Open history —"))
                );
            }
            "registry" => app.workspace_model.identity = uuid::Uuid::new_v4(),
            "workspace" => {
                app.durable = Arc::new((*app.durable).clone());
            }
            "backend" => {
                let Message::NotificationUi(Action::OpenNotice(clicked)) = &mut message else {
                    unreachable!()
                };
                clicked.backend = Some((
                    "different-device".into(),
                    "different-host".into(),
                    "different-executable".into(),
                ));
            }
            "closed" => app.close_attention(),
            _ => unreachable!(),
        }
        let original = app.durable.cards.clone();
        let focused = app.workspace.focused();
        reduce(&mut app, message);
        assert!(
            app.notification_ui.lifecycle_read.is_none(),
            "stale {changed} began a read"
        );
        assert!(app.notification_ui.lifecycle_activation.is_none());
        assert_eq!(app.durable.cards, original);
        assert_eq!(app.workspace.focused(), focused);
        assert!(
            app.notification_ui
                .pending
                .lock()
                .unwrap()
                .acknowledged
                .is_empty()
        );
    }
}

#[test]
fn current_row_refusal_during_modal_draft_or_work_retains_attention_for_explicit_reclick() {
    let mut renderer = super::super::tests::headless_renderer();
    for blocked in ["modal", "draft", "work"] {
        let mut app = fixture(1);
        open(&mut app);
        let message = click(&app, &mut renderer, notice(0).id);
        match blocked {
            "modal" => app.settings_reset_confirm = true,
            "draft" => {
                drop(app.update_automation_ui(crate::automation_ui::Action::New));
            }
            "work" => {
                let channel = ChannelId::new();
                app.workspace_model
                    .channels
                    .push(crate::workspace_model::ModelChannel {
                        id: channel,
                        repository: "/attention-work-fixture".into(),
                        title: "Original".into(),
                        title_source: Default::default(),
                        ordinal: 1,
                    });
                drop(app.open_channel_work(channel));
                assert!(app.channel_work_is_main());
            }
            _ => unreachable!(),
        }
        let original = app.durable.cards.clone();
        let focus = app.workspace.focused();
        reduce(&mut app, message);
        assert!(app.notification_ui.lifecycle_read.is_none(), "{blocked}");
        assert_eq!(app.durable.cards, original);
        assert_eq!(app.workspace.focused(), focus);
        no_ack(&app, notice(0).id);
        match blocked {
            "modal" => app.settings_reset_confirm = false,
            "draft" => {
                drop(app.update_automation_ui(crate::automation_ui::Action::CancelEdit));
            }
            "work" => {
                drop(app.escape_channel_work());
                assert!(!app.channel_work_is_main());
            }
            _ => unreachable!(),
        }
        open(&mut app);
        let message = click(&app, &mut renderer, notice(0).id);
        reduce(&mut app, message);
        assert!(
            app.notification_ui.lifecycle_read.is_some(),
            "new explicit {blocked} click remains possible"
        );
        no_ack(&app, notice(0).id);
    }
}

#[test]
fn explicit_review_row_keeps_existing_ack_semantics_while_journal_reading_does_not_ack() {
    let mut renderer = super::super::tests::headless_renderer();
    let mut app = fixture(0);
    let mut entry = notice(0);
    entry.event.evidence.kind = crate::notifications::Kind::ReviewRequested;
    entry.event.content = None;
    entry.event.target = Target::PullRequest(crate::notifications::review::ReviewRequest {
        host: "github.com".into(),
        port: None,
        owner: "fixture".into(),
        repository: "repository".into(),
        number: 1,
        viewer: "viewer-1".into(),
        request: "request-1".into(),
    });
    snapshot(&mut app, vec![entry.clone()]);
    open(&mut app);
    let (_, mut messages) = frame(
        &app,
        &mut renderer,
        420.0,
        false,
        Some("Open review — fixture/repository #1 — entry 1"),
    );
    no_ack(&app, entry.id);
    assert_eq!(messages.len(), 1);
    reduce(&mut app, messages.pop().unwrap());
    assert!(app.notification_ui.acknowledged.contains(&entry.id));
    assert!(
        app.notification_ui
            .pending
            .lock()
            .unwrap()
            .acknowledged
            .contains(&entry.id)
    );
    assert!(app.notification_ui.lifecycle_read.is_none());
}

#[test]
fn focused_original_save_refuses_queued_attention_and_popup_close_has_no_focus_effect() {
    const NAME: &str = "app::notification_ui::journal_tests::focused_original_save_refuses_queued_attention_and_popup_close_has_no_focus_effect";
    if crate::repository_widget::retained_save_tests::isolated_test(NAME) {
        return;
    }
    let mut renderer = super::super::tests::headless_renderer();
    let mut app = fixture(1);
    let original = super::super::tests::terminal_card_state();
    let card = original.id;
    let directory = tempfile::tempdir().unwrap();
    let checkout = directory.path().join("checkout");
    std::fs::create_dir(&checkout).unwrap();
    std::fs::write(checkout.join("file.txt"), "baseline").unwrap();
    let descriptor = RepositoryView {
        project: ProjectId::new(),
        repository: checkout.clone(),
        worktree: checkout.clone(),
        selected_file: None,
    };
    let mut runtime = RepositoryRuntime::new_directory(descriptor.clone());
    runtime.set_auto_save(false);
    let loaded = super::super::tests::task_app_message(repository_task(runtime.update(
        RepositoryAction::OpenFile {
            card,
            path: "file.txt".into(),
        },
    )));
    let Message::Repository(loaded) = loaded else {
        panic!("actual original file-read completion")
    };
    drop(runtime.update(loaded));
    assert_eq!(runtime.editor_text().as_deref(), Some("baseline"));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Insert('!')),
    }));
    assert!(runtime.is_dirty());
    let expected_saved = runtime.editor_text().unwrap();
    assert!(runtime.configure_save_recovery(Default::default(), vec![checkout]));
    let mut original = original;
    original.kind = CardKind::Repository(descriptor);
    Arc::make_mut(&mut app.durable).cards.push(original);
    Arc::make_mut(&mut app.durable).focused = Some(card);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    open(&mut app);
    let queued = click(&app, &mut renderer, notice(0).id);
    let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    let save = runtime.request_save(card, false);
    assert!(runtime.is_saving());
    assert!(app.focused_repository_is_saving());
    let original_cards = app.durable.cards.clone();
    reduce(&mut app, queued);
    assert!(app.notification_ui.lifecycle_read.is_none());
    assert_eq!(app.durable.cards, original_cards);
    assert_eq!(app.workspace.focused(), Some(card));
    no_ack(&app, notice(0).id);
    let close = app.update_chrome(super::super::chrome_ui::Action::Close);
    assert!(!app.attention_is_open());
    assert!(
        iced::runtime::task::into_stream(close).is_none(),
        "closing behind the save must not queue widget focus away from its owner"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("checkout/file.txt")).unwrap(),
        "baseline",
        "save was never polled"
    );
    let finished = super::super::tests::task_app_message(repository_task(save));
    let Message::Repository(finished) = finished else {
        panic!("real save completion")
    };
    assert!(matches!(
        &finished,
        RepositoryAction::RetainedSaveFinished { result: Ok(_), .. }
    ));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    drop(runtime.update(finished));
    assert!(!runtime.is_saving() && !runtime.is_dirty());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("checkout/file.txt")).unwrap(),
        expected_saved
    );
    open(&mut app);
    let fresh = click(&app, &mut renderer, notice(0).id);
    reduce(&mut app, fresh);
    assert!(
        app.notification_ui.lifecycle_read.is_some(),
        "successful original save permits a fresh explicit click"
    );
    no_ack(&app, notice(0).id);
}

#[test]
fn queued_attention_close_during_modal_does_not_queue_focus_into_covered_workspace() {
    let mut app = fixture(1);
    open(&mut app);
    app.settings_reset_confirm = true;
    let original = app.workspace.focused();
    let close = app.update_chrome(super::super::chrome_ui::Action::Close);
    assert!(!app.attention_is_open());
    assert!(iced::runtime::task::into_stream(close).is_none());
    assert!(app.settings_reset_confirm);
    assert_eq!(app.workspace.focused(), original);
    no_ack(&app, notice(0).id);
}

#[test]
fn retained_pointer_press_cannot_activate_the_incident_that_replaces_its_row() {
    fn step(
        app: &Cagents,
        renderer: &mut cosmic::Renderer,
        cache: Cache,
        cursor: mouse::Cursor,
        events: &[iced::Event],
        current: uuid::Uuid,
    ) -> (Cache, Vec<Message>, iced::Point) {
        let mut ui = UserInterface::build(
            app.attention_journal(),
            Size::new(420.0, 900.0),
            cache,
            renderer,
        );
        let mut messages = Vec::new();
        ui.update(&[], cursor, renderer, &mut clipboard::Null, &mut messages);
        assert!(messages.is_empty());
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            cursor,
        );
        let nodes = ui.a11y_nodes(cursor);
        let expected = label(app, current);
        let button = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some(expected.as_str())
            })
            .expect("current real row button");
        let bounds = button.node().bounds().unwrap();
        let point = iced::Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        ui.update(
            events,
            cursor,
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        (ui.into_cache(), messages, point)
    }

    let mut renderer = super::super::tests::headless_renderer();
    for change in [
        "replace-id",
        "replace-target",
        "host",
        "registry",
        "workspace",
    ] {
        let mut app = fixture(2);
        open(&mut app);
        let a = notice(0).id;
        let current = if change == "replace-id" {
            notice(1).id
        } else {
            a
        };
        let (cache, messages, point_a) = step(
            &app,
            &mut renderer,
            Cache::default(),
            mouse::Cursor::Unavailable,
            &[],
            a,
        );
        assert!(messages.is_empty());
        let pressed = iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
        let released = iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        let cursor = mouse::Cursor::Available(point_a);
        let (cache, messages, _) = step(&app, &mut renderer, cache, cursor, &[pressed.clone()], a);
        assert!(messages.is_empty(), "a press alone is not activation");
        match change {
            "replace-id" => snapshot(&mut app, vec![notice(1)]),
            "replace-target" => {
                let mut notices = app.notification_ui.snapshot.notices.clone();
                notices[0].event.target = Target::Automation {
                    id: automation(0),
                    sequence: 43,
                };
                snapshot(&mut app, notices);
            }
            "host" => {
                let mut host = app.notification_ui.host.clone().unwrap();
                host.host_incarnation = crate::model::HostIncarnation::new("replacement");
                let notices = app.notification_ui.snapshot.notices.clone();
                drop(app.update_notifications(Action::HostBound(host)));
                snapshot(&mut app, notices);
            }
            "registry" => app.workspace_model.identity = uuid::Uuid::new_v4(),
            "workspace" => app.durable = Arc::new((*app.durable).clone()),
            _ => unreachable!(),
        }
        // Keep the actual retained tree/cache. An unkeyed row preserves its
        // button's is_pressed while diff replaces its callback/authority.
        let (cache, messages, point_current) = step(
            &app,
            &mut renderer,
            cache,
            cursor,
            &[released.clone()],
            current,
        );
        assert_eq!(
            point_current, point_a,
            "current incident really occupies the pressed slot"
        );
        assert!(messages.is_empty(), "old press crossed {change}");
        assert!(app.notification_ui.lifecycle_read.is_none());
        assert!(app.durable.cards.is_empty());
        no_ack(&app, current);
        let (cache, messages, _) = step(&app, &mut renderer, cache, cursor, &[pressed], current);
        assert!(messages.is_empty());
        let (_, mut messages, _) = step(&app, &mut renderer, cache, cursor, &[released], current);
        assert_eq!(
            messages.len(),
            1,
            "fresh current click remains usable after {change}"
        );
        let Message::NotificationUi(Action::OpenNotice(clicked)) = &messages[0] else {
            panic!("real retained button callback");
        };
        assert_eq!(clicked.notice, current);
        let expected_target = app
            .notification_ui
            .snapshot
            .notices
            .iter()
            .find(|n| n.id == current)
            .unwrap()
            .event
            .target
            .clone();
        assert_eq!(clicked.target, expected_target);
        reduce(&mut app, messages.pop().unwrap());
        let Target::Automation {
            id: expected_id,
            sequence: expected_sequence,
        } = expected_target
        else {
            unreachable!()
        };
        let activation = app.notification_ui.lifecycle_activation.as_ref().unwrap();
        assert_eq!(
            (activation.id, activation.sequence),
            (expected_id, expected_sequence)
        );
        no_ack(&app, current);
    }
}

// Append inside app::notification_ui::journal_tests, after its existing parents.
// Reuses that child's fixture/snapshot/frame/click/no_ack helpers unchanged.
// Source-only proposed fragment: no test execution or RPC/native claim.

#[test]
fn public_attention_row_closes_only_its_popup_and_starts_exact_read_after_existing_guards() {
    let mut renderer = super::super::tests::headless_renderer();
    let mut app = fixture(1);
    let original_notice = notice(0);
    let (automation_id, lifecycle_sequence) = match &original_notice.event.target {
        Target::Automation { id, sequence } => (*id, *sequence),
        _ => panic!("automation fixture target"),
    };
    let proof = app.notification_ui.host.clone().unwrap();
    let original_snapshot = app.notification_ui.snapshot.clone();
    // Public observer ordering, not direct snapshot_host/readiness assignment.
    drop(app.update(Message::NotificationUi(Action::HostBound(proof.clone()))));
    drop(app.update(Message::NotificationUi(Action::Snapshot(original_snapshot))));
    assert!(app.attention_backend_current());
    drop(app.update(Message::ChromeUi(
        super::super::chrome_ui::Action::ToggleAttention,
    )));
    assert!(app.attention_is_open());
    let original_cards = app.durable.cards.clone();
    let original_focus = app.workspace.focused();
    let message = click(&app, &mut renderer, original_notice.id);
    let Message::NotificationUi(Action::OpenNotice(bound)) = &message else {
        panic!("real rendered row callback");
    };
    assert_eq!(bound.notice, original_notice.id);
    assert_eq!(bound.target, original_notice.event.target);
    assert_eq!(bound.host.as_ref(), Some(&proof));
    assert_eq!(bound.registry, app.workspace_model.identity);
    let read = app.update(message);
    let activation = app
        .notification_ui
        .lifecycle_activation
        .as_ref()
        .expect("Attention must not block its own accepted row");
    assert_eq!(
        (activation.id, activation.sequence),
        (automation_id, lifecycle_sequence)
    );
    assert_eq!(activation.host, proof);
    assert_eq!(activation.registry, app.workspace_model.identity);
    assert_eq!(activation.backend, lifecycle_backend());
    assert_eq!(activation.workspace.as_ptr(), Arc::as_ptr(&app.durable));
    assert_eq!(
        app.notification_ui.lifecycle_read,
        Some(activation.generation)
    );
    assert!(!app.attention_is_open());
    assert_eq!(
        app.durable.cards, original_cards,
        "read before utility creation"
    );
    assert_eq!(app.workspace.focused(), original_focus);
    no_ack(&app, original_notice.id);
    drop(read); // Deliberately unpolled: proves handler routing, not an RPC.

    // An exception for the owned Attention popup must not dismiss or bypass
    // other input owners. Existing focused-save parent stays unchanged too.
    for blocked in ["modal", "draft", "work"] {
        let mut app = fixture(1);
        let snapshot = app.notification_ui.snapshot.clone();
        drop(app.update(Message::NotificationUi(Action::Snapshot(snapshot))));
        drop(app.update(Message::ChromeUi(
            super::super::chrome_ui::Action::ToggleAttention,
        )));
        let queued = click(&app, &mut renderer, notice(0).id);
        match blocked {
            "modal" => app.settings_reset_confirm = true,
            "draft" => {
                drop(app.update_automation_ui(crate::automation_ui::Action::New));
                assert!(app.automation_ui.has_draft());
            }
            "work" => {
                let channel = ChannelId::new();
                app.workspace_model
                    .channels
                    .push(crate::workspace_model::ModelChannel {
                        id: channel,
                        repository: "/attention-work-fixture".into(),
                        title: "Original".into(),
                        title_source: Default::default(),
                        ordinal: 1,
                    });
                drop(app.open_channel_work(channel));
                assert!(app.channel_work_is_main());
            }
            _ => unreachable!(),
        }
        let cards = app.durable.cards.clone();
        let focus = app.workspace.focused();
        drop(app.update(queued));
        assert!(
            app.notification_ui.lifecycle_activation.is_none(),
            "{blocked}"
        );
        assert!(app.notification_ui.lifecycle_read.is_none(), "{blocked}");
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.workspace.focused(), focus);
        match blocked {
            "modal" => assert!(app.settings_reset_confirm),
            "draft" => assert!(app.automation_ui.has_draft()),
            "work" => assert!(app.channel_work_is_main()),
            _ => unreachable!(),
        }
        no_ack(&app, notice(0).id);
    }
}

#[test]
fn locally_read_session_stays_out_of_new_attention_after_stale_snapshot_without_acknowledging_automation()
 {
    let mut renderer = super::super::tests::headless_renderer();
    let mut app = super::super::tests::context_inspector_test_app(true);
    // Current Attention requires an accepted Registry identity before opening rows.
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.context_inspector_open = false;
    let card = app.workspace.focused().unwrap();
    let session = card_session(&app.durable.cards[0]).unwrap();
    let local = crate::notifications::Notice {
        id: uuid::Uuid::new_v4(),
        event: crate::notifications::Event::session(
            session,
            Provider::Claude,
            "/tmp".into(),
            crate::notifications::Evidence {
                key: "read-before-broker-ack".into(),
                kind: crate::notifications::Kind::Succeeded,
            },
        ),
        delivered: true,
        acknowledged: false,
        window: None,
    };
    let automation = notice(0);
    assert_ne!(local.id, automation.id);
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("attention-local-ack"),
        boot_id: crate::model::BootId::new("attention-local-ack"),
        host_incarnation: crate::model::HostIncarnation::new("attention-local-ack"),
    };
    let stale = Snapshot {
        notices: vec![local.clone(), automation.clone()],
        diagnostic: None,
    };
    drop(app.update(Message::NotificationUi(Action::HostBound(proof))));
    drop(app.update(Message::NotificationUi(Action::Snapshot(stale.clone()))));
    assert!(app.attention_backend_current());
    assert!(app.attention_notices().iter().any(|n| n.id == local.id));
    // The existing explicit local Mark Read path, not a fabricated broker ACK.
    drop(app.mark_cards_read(vec![card]));
    assert!(app.notification_ui.acknowledged_for_test(local.id));
    assert!(
        app.notification_ui
            .pending
            .lock()
            .unwrap()
            .acknowledged
            .contains(&local.id)
    );
    no_ack(&app, automation.id);

    // Broker's unchanged unacknowledged snapshot arrives before queued ACK.
    // Feed the real public message so accepted snapshot ownership is current.
    drop(app.update(Message::NotificationUi(Action::Snapshot(stale))));
    assert!(app.notification_ui.acknowledged_for_test(local.id));
    drop(app.update(Message::ChromeUi(
        super::super::chrome_ui::Action::ToggleAttention,
    )));
    assert!(app.attention_is_open());
    let visible = app
        .attention_notices()
        .iter()
        .map(|n| n.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(visible, BTreeSet::from([automation.id]));
    let (nodes, messages) = frame(&app, &mut renderer, 420.0, false, None);
    assert!(messages.is_empty(), "rendering is not activation");
    assert!(labels(&nodes).contains(&label(&app, automation.id)));
    no_ack(&app, automation.id);
    assert!(app.notification_ui.lifecycle_read.is_none());
    assert!(app.notification_ui.lifecycle_activation.is_none());
    drop(app.update(Message::ChromeUi(super::super::chrome_ui::Action::Close)));
    no_ack(&app, automation.id);
    assert_eq!(
        app.notification_ui.pending.lock().unwrap().acknowledged,
        BTreeSet::from([local.id]),
        "opening/reading/closing Attention must not ACK Automation",
    );
}
