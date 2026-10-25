use super::*;
use crate::account::import::{PhysicalIdentity, PublicationReceipt, SourceObservation};
use crate::app::tests::context_inspector_test_app;
use crate::model::{BootId, HostIncarnation, HostSessionId};

fn proof(name: &str) -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new(name),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("fixture-incarnation"),
    }
}
fn target() -> Target {
    Target {
        socket: "/private-fixture/import.sock".into(),
        proof: proof("initial"),
        profile: Profile::current(),
    }
}
fn observation() -> SourceObservation {
    SourceObservation {
        path: "/staged/auth.json".into(),
        parent: PhysicalIdentity {
            device: 1,
            inode: 2,
        },
        file: PhysicalIdentity {
            device: 1,
            inode: 3,
        },
        size: 100,
        mtime_seconds: 1,
        mtime_nanoseconds: 0,
        ctime_seconds: 1,
        ctime_nanoseconds: 0,
    }
}
fn act(state: &mut State, action: Action) -> Option<Request> {
    state.update(action, Profile::current())
}
fn inspected() -> State {
    let mut state = State::default();
    assert!(act(&mut state, Action::Open).is_none());
    let owner = state.owner;
    for input in [
        Action::Path(owner, "~/staged/auth.json".into()),
        Action::Email(owner, "fixture@example.test".into()),
        Action::CopyConsent(owner, true),
        Action::SessionConsent(owner, true),
    ] {
        assert!(act(&mut state, input).is_none());
    }
    let request = act(&mut state, Action::Inspect(owner)).unwrap();
    let response = Response {
        target: target(),
        reply: ImportReply::Inspected(ImportInspection {
            destination: target().proof,
            observation: observation(),
        }),
    };
    assert!(
        act(
            &mut state,
            Action::Finished {
                sequence: request.sequence,
                result: Ok(response)
            }
        )
        .is_none()
    );
    state
}
fn prepared() -> (State, Request) {
    let mut state = inspected();
    let owner = state.owner;
    let request = act(&mut state, Action::Prepare(owner)).unwrap();
    (state, request)
}
fn status(plan: &ImportPlan, stage: ImportStage) -> ImportStatus {
    ImportStatus {
        observed_destination: target().proof,
        plan: plan.clone(),
        receipt: Some(PublicationReceipt {
            operation: plan.operation,
            home: PhysicalIdentity {
                device: 4,
                inode: 5,
            },
            file: PhysicalIdentity {
                device: 4,
                inode: 6,
            },
            size: 100,
        }),
        stage,
        uncertain: false,
        publication: ImportPublication::Present,
        registered: (stage == ImportStage::Registered)
            .then(|| format!("codex/{}", plan.operation).parse().unwrap()),
        coding_profile: None,
    }
}
fn finish(state: &mut State, request: &Request, status: ImportStatus) {
    assert!(
        act(
            state,
            Action::Finished {
                sequence: request.sequence,
                result: Ok(Response {
                    target: target(),
                    reply: ImportReply::Status(status)
                })
            }
        )
        .is_none()
    );
}

#[test]
fn inspect_and_prepare_require_separate_explicit_intents_and_keep_the_raw_path() {
    let mut state = State::default();
    act(&mut state, Action::Open);
    let owner = state.owner;
    assert!(act(&mut state, Action::Prepare(owner)).is_none());
    assert!(state.plan.is_none() && state.pending.is_none());
    act(&mut state, Action::Close(owner));
    assert!(state.plan.is_none());
    state = inspected();
    assert_eq!(state.path, "~/staged/auth.json");
    assert!(state.plan.is_none() && state.pending.is_none());
    let owner = state.owner;
    act(&mut state, Action::SessionConsent(owner, false));
    assert!(act(&mut state, Action::Prepare(owner)).is_none());
    act(&mut state, Action::SessionConsent(owner, true));
    act(&mut state, Action::WorkspaceWrite(owner, true));
    act(&mut state, Action::Network(owner, true));
    let request = act(&mut state, Action::Prepare(owner)).unwrap();
    let plan = state.plan.as_ref().unwrap();
    assert!(!plan.operation.is_nil());
    assert_eq!(
        plan.profile,
        CodingProfile::WorkspaceWrite {
            network_access: true
        }
    );
    assert_eq!(plan.observation, observation());
    assert_eq!(plan.expected_email, "fixture@example.test");
    assert_eq!(
        wire_operation(&request, &target().proof).unwrap(),
        ImportOperation::Prepare { plan: plan.clone() }
    );
    assert!(wire_operation(&request, &proof("restarted")).is_err());
}

#[test]
fn closed_or_replaced_form_cannot_send_but_accepted_completion_preserves_original_operation() {
    let (mut state, request) = prepared();
    let old_owner = state.owner;
    let plan = state.plan.clone().unwrap();
    assert!(act(&mut state, Action::Close(old_owner)).is_none());
    assert!(state.pending.is_some());
    for action in [
        Action::Prepare(old_owner),
        Action::Verify(old_owner),
        Action::Status(old_owner),
    ] {
        assert!(act(&mut state, action).is_none());
    }
    finish(&mut state, &request, status(&plan, ImportStage::Published));
    assert!(!state.visible);
    assert_eq!(state.plan.as_ref(), Some(&plan));
    assert_eq!(state.status.as_ref().unwrap().stage, ImportStage::Published);
    assert!(act(&mut state, Action::Open).is_none());
    let owner = state.owner;
    assert_ne!(old_owner, owner);
    assert!(act(&mut state, Action::Verify(old_owner)).is_none());
    assert!(
        act(
            &mut state,
            Action::Path(owner, "/replacement/auth.json".into())
        )
        .is_none()
    );
    assert_eq!(state.plan.as_ref(), Some(&plan));
    let request = act(&mut state, Action::Verify(owner)).unwrap();
    assert_eq!(
        wire_operation(&request, &proof("fresh-current")).unwrap(),
        ImportOperation::Verify {
            operation: plan.operation,
            destination: proof("fresh-current")
        }
    );
    assert_eq!(state.plan.as_ref(), Some(&plan));
}

#[test]
fn lost_reply_never_enables_prepare_resend_and_status_or_verify_keep_the_same_uuid() {
    let (mut state, request) = prepared();
    let plan = state.plan.clone().unwrap();
    let owner = state.owner;
    act(
        &mut state,
        Action::Finished {
            sequence: request.sequence,
            result: Err(Failure::Unavailable),
        },
    );
    assert!(act(&mut state, Action::Prepare(owner)).is_none());
    assert!(
        act(&mut state, Action::Verify(owner)).is_none(),
        "unconfirmed publication is not a Verify permit"
    );
    let read = act(&mut state, Action::Status(owner)).unwrap();
    assert_eq!(
        wire_operation(&read, &proof("fresh")).unwrap(),
        ImportOperation::Status {
            operation: plan.operation
        }
    );
    let mut prepared = status(&plan, ImportStage::Prepared);
    prepared.uncertain = true;
    finish(&mut state, &read, prepared);
    assert!(state.status.as_ref().unwrap().uncertain);
    let verify = act(&mut state, Action::Verify(owner)).unwrap();
    assert!(
        matches!(wire_operation(&verify,&proof("fresh")).unwrap(),ImportOperation::Verify{operation,..} if operation==plan.operation)
    );
    assert_eq!(state.plan.as_ref(), Some(&plan));
}

#[test]
fn wrong_completion_sequence_plan_profile_home_or_registered_account_cannot_replace_the_original() {
    let (mut state, request) = prepared();
    let plan = state.plan.clone().unwrap();
    let good = status(&plan, ImportStage::Published);
    act(
        &mut state,
        Action::Finished {
            sequence: request.sequence + 1,
            result: Ok(Response {
                target: target(),
                reply: ImportReply::Status(good.clone()),
            }),
        },
    );
    assert!(state.pending.is_some());
    assert!(state.status.is_none());
    finish(&mut state, &request, good.clone());
    let owner = state.owner;
    let mut wrong_plan = good.clone();
    wrong_plan.plan.operation = Uuid::from_u128(123);
    let mut wrong_receipt = good.clone();
    wrong_receipt.receipt.as_mut().unwrap().home.inode += 1;
    let mut wrong_account = status(&plan, ImportStage::Registered);
    wrong_account.registered = Some(
        "codex/00000000-0000-0000-0000-000000000012"
            .parse()
            .unwrap(),
    );
    for bad in [wrong_plan, wrong_receipt, wrong_account] {
        let read = act(&mut state, Action::Status(owner)).unwrap();
        finish(&mut state, &read, bad);
        assert_eq!(state.status.as_ref(), Some(&good));
        assert_eq!(state.plan.as_ref(), Some(&plan));
    }
    let read = act(&mut state, Action::Status(owner)).unwrap();
    let mut different = target();
    different.profile = Profile(Some((
        "Other".into(),
        "other-host".into(),
        "other-cli".into(),
    )));
    act(
        &mut state,
        Action::Finished {
            sequence: read.sequence,
            result: Ok(Response {
                target: different,
                reply: ImportReply::Status(good.clone()),
            }),
        },
    );
    assert_eq!(state.status.as_ref(), Some(&good));
    assert!(
        state
            .update(
                Action::Verify(owner),
                Profile(Some((
                    "Other".into(),
                    "other-host".into(),
                    "other-cli".into()
                )))
            )
            .is_none()
    );
}

#[test]
fn editing_invalidates_inspection_and_no_result_or_capacity_error_dispatches_another_stage() {
    let mut state = inspected();
    let owner = state.owner;
    act(&mut state, Action::Path(owner, "/new/auth.json".into()));
    assert!(state.inspection.is_none());
    assert!(!state.can_prepare());
    let request = act(&mut state, Action::Inspect(owner)).unwrap();
    assert!(
        act(
            &mut state,
            Action::Finished {
                sequence: request.sequence,
                result: Err(Failure::Journal)
            }
        )
        .is_none()
    );
    assert!(state.pending.is_none() && state.plan.is_none());
    assert!(state.problem.unwrap().contains("16 retained operations"));
    assert!(
        state
            .problem
            .unwrap()
            .contains("Completion does not free a slot")
    );
    let (mut state, request) = prepared();
    let plan = state.plan.clone().unwrap();
    finish(&mut state, &request, status(&plan, ImportStage::Registered));
    assert!(
        state.pending.is_none(),
        "registration never auto-dispatches a check, provider request or login"
    );
    assert_eq!(state.status.unwrap().coding_profile, None);
}

// Real widget callbacks at the narrow pane width, without opening a daemon or
// interpreting provider/cache data. All emitted messages remain intact.
fn click(mut element: Element<'_, Message>, label: &str) -> Vec<Message> {
    use iced::advanced::{Layout, clipboard, layout, widget::Tree};
    use iced::runtime::{UserInterface, user_interface::Cache};
    use iced::{Event, Point, Size, mouse};
    let mut renderer = crate::app::tests::headless_renderer();
    let size = Size::new(360.0, 2200.0);
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, size),
    );
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let bounds = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| node.node().label() == Some(label))
        .unwrap_or_else(|| panic!("missing actual control {label}"))
        .node()
        .bounds()
        .unwrap();
    assert!(
        bounds.x0 >= 0.0
            && bounds.x1 <= 360.0
            && bounds.x1 > bounds.x0
            && bounds.y1 > bounds.y0
            && bounds.y1 <= 2200.0,
        "{label}: {bounds:?}"
    );
    let point = Point::new(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    let mut ui = UserInterface::build(element, size, Cache::new(), &mut renderer);
    let mut messages = vec![];
    let _ = ui.update(
        &[
            Event::Mouse(mouse::Event::CursorMoved { position: point }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ],
        mouse::Cursor::Available(point),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    messages
}

#[test]
fn actual_import_entry_and_prepare_controls_emit_owned_actions_and_hidden_callbacks_are_refused() {
    let mut app = context_inspector_test_app(false);
    let messages = click(
        table::add_menu(&app.account_ui, false),
        "Import staged ChatGPT cache",
    );
    assert!(matches!(
        messages.as_slice(),
        [Message::AccountUi(super::super::Action::Import(
            Action::Open
        ))]
    ));
    for message in messages {
        drop(app.update(message));
    }
    assert!(app.account_ui.import.visible);
    app.account_ui.import = inspected();
    let owner = app.account_ui.import.owner;
    let messages = click(
        view(&app).unwrap().map(super::super::pane_message),
        "Prepare import",
    );
    assert!(
        matches!(messages.as_slice(),[Message::UtilityPaneUi(message)] if matches!(message.as_ref(),Message::AccountUi(super::super::Action::Import(Action::Prepare(n))) if *n==owner))
    );
    // Switch the real App to main Work after capturing the Accounts callbacks.
    // The task is dropped without executing a connection or receiving a snapshot.
    let channel = ChannelId::new();
    app.workspace_model
        .channels
        .push(crate::workspace_model::ModelChannel {
            id: channel,
            repository: "/fixture-project".into(),
            title: "Fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
        });
    drop(app.update(Message::OpenChannelWork(channel)));
    assert!(app.channel_work_is_main());
    // Dispatch the entire native vector through real App; hidden pane input is refused.
    for message in messages {
        drop(app.update(message));
    }
    assert!(app.account_ui.import.plan.is_none());
    assert!(app.account_ui.import.pending.is_none());
    let messages = click(view(&app).unwrap(), "Prepare import");
    assert!(
        matches!(messages.as_slice(),[Message::AccountUi(super::super::Action::Import(Action::Prepare(n)))] if *n==owner)
    );
    // Own the exact accepted request in the pure state reducer, not an executor.
    for message in messages {
        let Message::AccountUi(super::super::Action::Import(action)) = message else {
            panic!("unexpected native callback")
        };
        assert!(act(&mut app.account_ui.import, action).is_some());
    }
    assert!(app.account_ui.import.plan.is_some());
}

#[test]
fn accepted_completion_is_not_pane_input_and_dismissal_does_not_change_defaults_or_select_an_account()
 {
    let mut app = context_inspector_test_app(false);
    let before = app.account_ui.registry.clone();
    let (state, request) = prepared();
    let plan = state.plan.clone().unwrap();
    let owner = state.owner;
    app.account_ui.import = state;
    drop(app.update_account_import(Action::Close(owner)));
    let reply = status(&plan, ImportStage::Published);
    let completion = message(Action::Finished {
        sequence: request.sequence,
        result: Ok(Response {
            target: target(),
            reply: ImportReply::Status(reply.clone()),
        }),
    });
    assert!(matches!(
        &completion,
        Message::AccountUi(super::super::Action::Import(Action::Finished { .. }))
    ));
    drop(app.update(completion));
    assert!(!app.account_ui.import.visible);
    assert_eq!(app.account_ui.import.status.as_ref(), Some(&reply));
    assert_eq!(app.account_ui.registry, before);
    assert!(app.account_ui.import.pending.is_none());
}

#[test]
fn held_prepare_press_cannot_migrate_to_a_reopened_import_form() {
    use iced::advanced::clipboard;
    use iced::runtime::{UserInterface, user_interface::Cache};
    use iced::{Event, Point, Size, mouse};
    let mut app = context_inspector_test_app(false);
    app.account_ui.import = inspected();
    let old_owner = app.account_ui.import.owner;
    let mut renderer = crate::app::tests::headless_renderer();
    let size = Size::new(360.0, 2200.0);
    let mut ui = UserInterface::build(view(&app).unwrap(), size, Cache::new(), &mut renderer);
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let bounds = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| node.node().label() == Some("Prepare import"))
        .unwrap()
        .node()
        .bounds()
        .unwrap();
    let point = Point::new(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    let mut messages = vec![];
    let _ = ui.update(
        &[
            Event::Mouse(mouse::Event::CursorMoved { position: point }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ],
        mouse::Cursor::Available(point),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(messages.is_empty());
    let cache = ui.into_cache();
    act(&mut app.account_ui.import, Action::Close(old_owner));
    act(&mut app.account_ui.import, Action::Open);
    let owner = app.account_ui.import.owner;
    assert_ne!(owner, old_owner);
    let mut ui = UserInterface::build(view(&app).unwrap(), size, cache, &mut renderer);
    let _ = ui.update(
        &[Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        ))],
        mouse::Cursor::Available(point),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        messages.is_empty(),
        "old held press must not prepare a new form"
    );
    let _ = ui.update(
        &[
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ],
        mouse::Cursor::Available(point),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        matches!(messages.as_slice(),[Message::AccountUi(super::super::Action::Import(Action::Prepare(actual)))] if *actual==owner)
    );
    let _ = ui.into_cache();
    for message in messages {
        let Message::AccountUi(super::super::Action::Import(action)) = message else {
            panic!("unexpected callback")
        };
        assert!(act(&mut app.account_ui.import, action).is_some());
    }
    assert!(app.account_ui.import.plan.is_some());
}
