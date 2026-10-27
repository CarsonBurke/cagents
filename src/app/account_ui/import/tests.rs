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
        authority: crate::agent_execution::Authority::default(),
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
    // Discriminate the FIRST response, before a previous receipt can protect it.
    for reserved_with_receipt in [false, true] {
        let (mut first, request) = prepared();
        let plan = first.plan.clone().unwrap();
        let mut inconsistent = status(&plan, ImportStage::Published);
        if reserved_with_receipt {
            inconsistent.stage = ImportStage::Reserved;
        } else {
            inconsistent.receipt.as_mut().unwrap().size += 1;
        }
        finish(&mut first, &request, inconsistent);
        assert!(
            first.status.is_none(),
            "inconsistent first publication metadata must not be adopted"
        );
        assert_eq!(first.plan.as_ref(), Some(&plan));
        assert!(first.pending.is_none());
        let owner = first.owner;
        assert!(
            act(&mut first, Action::Prepare(owner)).is_none(),
            "a refused response never authorizes reseeding"
        );
    }
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
    let mut absolute = State::default();
    act(&mut absolute, Action::Open);
    let owner = absolute.owner;
    act(
        &mut absolute,
        Action::Path(owner, "/selected/auth.json".into()),
    );
    let inspected_request = act(&mut absolute, Action::Inspect(owner)).unwrap();
    let mut foreign = observation();
    foreign.path = "/different/auth.json".into();
    act(
        &mut absolute,
        Action::Finished {
            sequence: inspected_request.sequence,
            result: Ok(Response {
                target: target(),
                reply: ImportReply::Inspected(ImportInspection {
                    destination: target().proof,
                    observation: foreign,
                }),
            }),
        },
    );
    assert!(absolute.inspection.is_none());
    assert!(!absolute.can_prepare());
    assert_eq!(absolute.path, "/selected/auth.json");
    let inspected_request = act(&mut absolute, Action::Inspect(owner)).unwrap();
    let mut exact = observation();
    exact.path = "/selected/auth.json".into();
    act(
        &mut absolute,
        Action::Finished {
            sequence: inspected_request.sequence,
            result: Ok(Response {
                target: target(),
                reply: ImportReply::Inspected(ImportInspection {
                    destination: target().proof,
                    observation: exact,
                }),
            }),
        },
    );
    assert!(
        absolute.inspection.is_some(),
        "the exact absolute selection remains supported"
    );
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

    // Make a reload observable without executing it or touching any Store.
    // Only an accepted explicit Verify may create the existing reload task.
    app.account_ui.store = Some(Store::at("/not-opened/import-ui-fixture/registry"));
    let (mut known, prepare) = prepared();
    let original = known.plan.clone().unwrap();
    finish(
        &mut known,
        &prepare,
        status(&original, ImportStage::Published),
    );
    let owner = known.owner;
    let verify = act(&mut known, Action::Verify(owner)).unwrap();
    finish(
        &mut known,
        &verify,
        status(&original, ImportStage::Registered),
    );
    app.account_ui.import = known;
    for list in [false, true] {
        let request = act(
            &mut app.account_ui.import,
            if list {
                Action::Load(owner)
            } else {
                Action::Status(owner)
            },
        )
        .unwrap();
        let current = status(&original, ImportStage::Registered);
        let reply = if list {
            ImportReply::Listed(vec![current])
        } else {
            ImportReply::Status(current)
        };
        let task = app.update_account_import(Action::Finished {
            sequence: request.sequence,
            result: Ok(Response {
                target: target(),
                reply,
            }),
        });
        assert_eq!(
            task.units(),
            0,
            "metadata completion must not reload/probe an old registered account"
        );
        drop(task);
    }
    let request = act(&mut app.account_ui.import, Action::Verify(owner)).unwrap();
    let task = app.update_account_import(Action::Finished {
        sequence: request.sequence + 1,
        result: Ok(Response {
            target: target(),
            reply: ImportReply::Status(status(&original, ImportStage::Registered)),
        }),
    });
    assert_eq!(
        task.units(),
        0,
        "wrong sequence cannot create registry work"
    );
    drop(task);
    assert!(app.account_ui.import.pending.is_some());
    let mut inconsistent = status(&original, ImportStage::Registered);
    inconsistent.receipt.as_mut().unwrap().size += 1;
    let task = app.update_account_import(Action::Finished {
        sequence: request.sequence,
        result: Ok(Response {
            target: target(),
            reply: ImportReply::Status(inconsistent),
        }),
    });
    assert_eq!(
        task.units(),
        0,
        "refused Verify metadata cannot create a reload"
    );
    drop(task);
    let request = act(&mut app.account_ui.import, Action::Verify(owner)).unwrap();
    let completion = Action::Finished {
        sequence: request.sequence,
        result: Ok(Response {
            target: target(),
            reply: ImportReply::Status(status(&original, ImportStage::Registered)),
        }),
    };
    let task = app.update_account_import(completion.clone());
    assert!(
        task.units() > 0,
        "matching explicit Verify keeps its registration reload"
    );
    drop(task); // Never poll the filesystem/provider future in this test.
    let task = app.update_account_import(completion);
    assert_eq!(
        task.units(),
        0,
        "duplicate completion must not reload an old registration"
    );
    drop(task);
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

#[test]
fn retained_list_resumes_an_original_plan_after_restart_and_verify_uses_the_fresh_proof() {
    let (original, _) = prepared();
    let plan = original.plan.clone().unwrap();
    let mut retained = status(&plan, ImportStage::Published);
    retained.observed_destination = proof("restarted-daemon");
    let mut response_target = target();
    response_target.proof = retained.observed_destination.clone();
    let mut state = State::default();
    assert!(act(&mut state, Action::Open).is_none());
    assert!(
        state.pending.is_none(),
        "Open never contacts a provider or automatically resumes a mutation"
    );
    let owner = state.owner;
    let load = act(&mut state, Action::Load(owner)).unwrap();
    assert_eq!(
        wire_operation(&load, &proof("restarted-daemon")).unwrap(),
        ImportOperation::List
    );
    assert!(
        act(
            &mut state,
            Action::Finished {
                sequence: load.sequence,
                result: Ok(Response {
                    target: response_target.clone(),
                    reply: ImportReply::Listed(vec![retained.clone()])
                })
            }
        )
        .is_none()
    );
    assert!(
        state.plan.is_none() && state.pending.is_none(),
        "discovery does not choose or dispatch an operation"
    );
    let discovery = state.discovery;
    assert!(
        act(
            &mut state,
            Action::Resume {
                owner,
                discovery: discovery + 1,
                operation: plan.operation
            }
        )
        .is_none()
    );
    assert!(state.plan.is_none());
    let mut mounted = context_inspector_test_app(false);
    mounted.account_ui.import = state;
    let callbacks = click(view(&mounted).unwrap(), "Resume import");
    assert!(
        matches!(callbacks.as_slice(), [Message::AccountUi(super::super::Action::Import(Action::Resume {owner: actual_owner, discovery: actual_discovery, operation}))] if *actual_owner == owner && *actual_discovery == discovery && *operation == plan.operation)
    );
    for callback in callbacks {
        let Message::AccountUi(super::super::Action::Import(action)) = callback else {
            panic!("unexpected callback")
        };
        assert!(act(&mut mounted.account_ui.import, action).is_none());
    }
    let mut state = std::mem::take(&mut mounted.account_ui.import);
    assert_eq!(state.plan.as_ref(), Some(&plan));
    assert_eq!(state.status.as_ref(), Some(&retained));
    assert!(state.pending.is_none());
    let resumed_owner = state.owner;
    assert_ne!(owner, resumed_owner);
    assert!(
        act(&mut state, Action::Prepare(resumed_owner)).is_none(),
        "restored plans cannot seed another cache"
    );
    assert!(
        act(&mut state, Action::Verify(owner)).is_none(),
        "old rendered callbacks cannot verify a resumed form"
    );
    let verify = act(&mut state, Action::Verify(resumed_owner)).unwrap();
    assert_eq!(
        wire_operation(&verify, &proof("next-fresh-connection")).unwrap(),
        ImportOperation::Verify {
            operation: plan.operation,
            destination: proof("next-fresh-connection")
        }
    );
    assert_eq!(state.plan.as_ref().unwrap().destination, proof("initial"));
    // A legitimate historical Registered record may outlive its account row.
    let mut historical = status(&plan, ImportStage::Registered);
    historical.registered = None;
    historical.observed_destination = response_target.proof.clone();
    let mut app = context_inspector_test_app(false);
    assert!(valid_status(
        &historical,
        &plan,
        &response_target.proof,
        None
    ));
    app.account_ui.import.plan = Some(plan);
    app.account_ui.import.status = Some(historical);
    assert!(
        app.account_ui
            .import
            .registered_account(&app.account_ui)
            .is_none()
    );
}

#[test]
fn retained_discovery_is_bounded_and_cannot_drop_or_replace_an_unacknowledged_local_operation() {
    for reserved_with_receipt in [false, true] {
        let (source, _) = prepared();
        let plan = source.plan.unwrap();
        let mut inconsistent = status(&plan, ImportStage::Published);
        if reserved_with_receipt {
            inconsistent.stage = ImportStage::Reserved;
        } else {
            inconsistent.receipt.as_mut().unwrap().size += 1;
        }
        let mut fresh = State::default();
        act(&mut fresh, Action::Open);
        let owner = fresh.owner;
        let request = act(&mut fresh, Action::Load(owner)).unwrap();
        act(
            &mut fresh,
            Action::Finished {
                sequence: request.sequence,
                result: Ok(Response {
                    target: target(),
                    reply: ImportReply::Listed(vec![inconsistent]),
                }),
            },
        );
        assert!(
            fresh.retained.is_empty() && fresh.plan.is_none(),
            "first discovery must refuse inconsistent original publication metadata"
        );
        assert_eq!(fresh.discovery, 0);
    }
    let (mut state, prepare) = prepared();
    let plan = state.plan.clone().unwrap();
    let owner = state.owner;
    act(
        &mut state,
        Action::Finished {
            sequence: prepare.sequence,
            result: Err(Failure::Unavailable),
        },
    );
    let load = act(&mut state, Action::Load(owner)).unwrap();
    assert!(
        act(
            &mut state,
            Action::Finished {
                sequence: load.sequence,
                result: Ok(Response {
                    target: target(),
                    reply: ImportReply::Listed(vec![])
                })
            }
        )
        .is_none()
    );
    assert_eq!(state.plan.as_ref(), Some(&plan));
    assert!(state.local_uncertain && state.status.is_none());
    assert!(
        state
            .problem
            .unwrap()
            .contains("did not confirm your local operation")
    );
    let mut other = status(&plan, ImportStage::Published);
    other.plan.operation = Uuid::from_u128(701);
    other.receipt.as_mut().unwrap().operation = other.plan.operation;
    let load = act(&mut state, Action::Load(owner)).unwrap();
    act(
        &mut state,
        Action::Finished {
            sequence: load.sequence,
            result: Ok(Response {
                target: target(),
                reply: ImportReply::Listed(vec![other.clone()]),
            }),
        },
    );
    let discovery = state.discovery;
    assert!(
        act(
            &mut state,
            Action::Resume {
                owner,
                discovery,
                operation: other.plan.operation
            }
        )
        .is_none()
    );
    assert_eq!(state.plan.as_ref(), Some(&plan));
    assert!(act(&mut state, Action::New(owner)).is_none());
    assert_eq!(state.plan.as_ref(), Some(&plan));
    assert!(!state.can_new());
    let retained = state.retained.clone();
    for listed in [
        vec![other.clone(), other.clone()],
        (1..=17)
            .map(|n| {
                let mut row = other.clone();
                row.plan.operation = Uuid::from_u128(800 + n);
                row.receipt.as_mut().unwrap().operation = row.plan.operation;
                row
            })
            .collect(),
    ] {
        let load = act(&mut state, Action::Load(owner)).unwrap();
        act(
            &mut state,
            Action::Finished {
                sequence: load.sequence,
                result: Ok(Response {
                    target: target(),
                    reply: ImportReply::Listed(listed),
                }),
            },
        );
        assert_eq!(
            state.retained, retained,
            "ambiguous or oversized discovery must preserve the previous list"
        );
        assert_eq!(state.plan.as_ref(), Some(&plan));
    }
    let read = act(&mut state, Action::Status(owner)).unwrap();
    assert_eq!(
        wire_operation(&read, &proof("fresh")).unwrap(),
        ImportOperation::Status {
            operation: plan.operation
        }
    );
}

#[test]
fn new_import_requires_acknowledged_registration_and_retires_the_old_form_owner() {
    let (mut state, prepare) = prepared();
    let plan = state.plan.clone().unwrap();
    let owner = state.owner;
    assert!(act(&mut state, Action::New(owner)).is_none());
    assert_eq!(state.plan.as_ref(), Some(&plan));
    act(
        &mut state,
        Action::Finished {
            sequence: prepare.sequence,
            result: Err(Failure::Unavailable),
        },
    );
    assert!(act(&mut state, Action::New(owner)).is_none());
    assert_eq!(state.plan.as_ref(), Some(&plan));
    // Match the HOST journal's real trajectory: uncertainty remains true
    // through Prepared/Published and the later acknowledged Registered stage.
    // Journal::mark_registered never clears that historical audit evidence.
    for stage in [ImportStage::Prepared, ImportStage::Published] {
        let request = act(&mut state, Action::Status(owner)).unwrap();
        let mut uncertain = status(&plan, stage);
        uncertain.uncertain = true;
        finish(&mut state, &request, uncertain);
        assert!(!state.can_new());
        assert!(act(&mut state, Action::New(owner)).is_none());
        assert_eq!(state.plan.as_ref(), Some(&plan));
    }
    let verify = act(&mut state, Action::Verify(owner)).unwrap();
    assert!(
        !state.can_new(),
        "an outstanding Verify is not a terminal ACK"
    );
    act(
        &mut state,
        Action::Finished {
            sequence: verify.sequence,
            result: Err(Failure::Unavailable),
        },
    );
    assert!(!state.can_new(), "a lost Verify reply keeps local custody");
    let request = act(&mut state, Action::Status(owner)).unwrap();
    let mut historical = status(&plan, ImportStage::Registered);
    historical.uncertain = true;
    let mut wrong_receipt = historical.clone();
    wrong_receipt.receipt.as_mut().unwrap().file.inode += 1;
    finish(&mut state, &request, wrong_receipt);
    assert!(
        !state.can_new(),
        "a different receipt cannot resolve the original"
    );
    assert!(state.local_uncertain);
    let request = act(&mut state, Action::Status(owner)).unwrap();
    act(
        &mut state,
        Action::Finished {
            sequence: request.sequence + 1,
            result: Ok(Response {
                target: target(),
                reply: ImportReply::Status(historical.clone()),
            }),
        },
    );
    assert!(
        state.pending.is_some() && !state.can_new(),
        "a stale ACK cannot release the current request"
    );
    finish(&mut state, &request, historical.clone());
    assert!(state.can_new());
    assert!(!state.local_uncertain);
    assert!(
        state.status.as_ref().unwrap().uncertain,
        "historical audit evidence is not erased"
    );
    // The durable original remains available in discovery, without deletion,
    // expiration or pretending that completion frees one of the 16 slots.
    state.retained = vec![historical.clone()];
    let mut mounted = context_inspector_test_app(false);
    mounted.account_ui.import = state;
    let callbacks = click(view(&mounted).unwrap(), "New import");
    assert!(
        matches!(callbacks.as_slice(), [Message::AccountUi(super::super::Action::Import(Action::New(actual)))] if *actual == owner)
    );
    for callback in callbacks {
        let Message::AccountUi(super::super::Action::Import(action)) = callback else {
            panic!("unexpected callback")
        };
        assert!(act(&mut mounted.account_ui.import, action).is_none());
    }
    let mut state = std::mem::take(&mut mounted.account_ui.import);
    assert!(state.plan.is_none() && state.status.is_none() && state.pending.is_none());
    assert!(
        state.path.is_empty()
            && state.email.is_empty()
            && !state.copy
            && !state.session
            && !state.network
            && !state.write
    );
    assert_ne!(state.owner, owner);
    assert_eq!(state.retained, vec![historical]);
    assert!(
        act(
            &mut state,
            Action::Path(owner, "/old-callback/auth.json".into())
        )
        .is_none()
    );
    assert!(state.path.is_empty());
    let current = state.owner;
    assert!(act(&mut state, Action::Path(current, "/new/auth.json".into())).is_none());
    assert_eq!(state.path, "/new/auth.json");
    assert!(
        state.plan.is_none(),
        "New and editing do not mint an operation UUID"
    );
}

#[test]
fn restored_registered_audit_uncertainty_allows_a_separate_import_without_account_readiness() {
    for current_account in [false, true] {
        let original = inspected().inspection.unwrap().1;
        let plan = ImportPlan {
            operation: Uuid::from_u128(991),
            destination: original.destination,
            observation: original.observation,
            expected_email: "fixture@example.test".into(),
            consent: CopyConsent {
                staged_file_only: true,
                source_session_limit_acknowledged: true,
            },
            profile: CodingProfile::Unconfigured,
        };
        let mut historical = status(&plan, ImportStage::Registered);
        historical.uncertain = true;
        if !current_account {
            historical.registered = None;
        }
        let mut state = State::default();
        act(&mut state, Action::Open);
        let initial_owner = state.owner;
        let load = act(&mut state, Action::Load(initial_owner)).unwrap();
        assert!(
            !state.accept(
                load.sequence,
                Ok(Response {
                    target: target(),
                    reply: ImportReply::Listed(vec![historical.clone()]),
                })
            ),
            "List is metadata-only and never schedules registry/provider work"
        );
        assert!(state.plan.is_none());
        let owner = state.owner;
        let discovery = state.discovery;
        assert!(
            act(
                &mut state,
                Action::Resume {
                    owner,
                    discovery,
                    operation: plan.operation,
                }
            )
            .is_none()
        );
        assert_eq!(state.plan.as_ref(), Some(&plan));
        assert_eq!(state.status.as_ref(), Some(&historical));
        assert!(state.can_new());
        assert!(!state.local_uncertain);
        assert!(state.status.as_ref().unwrap().uncertain);
        let resumed_owner = state.owner;
        assert_ne!(resumed_owner, owner);

        // An older acknowledged status must not mask a NEW unanswered Verify.
        let verify = act(&mut state, Action::Verify(resumed_owner)).unwrap();
        assert!(state.local_uncertain && !state.can_new());
        act(
            &mut state,
            Action::Finished {
                sequence: verify.sequence,
                result: Err(Failure::Unavailable),
            },
        );
        assert_eq!(state.status.as_ref(), Some(&historical));
        assert!(state.local_uncertain && !state.can_new());
        let mut other = historical.clone();
        other.plan.operation = Uuid::from_u128(992);
        other.receipt.as_mut().unwrap().operation = other.plan.operation;
        if current_account {
            other.registered = Some(format!("codex/{}", other.plan.operation).parse().unwrap());
        }
        let listed = vec![historical.clone(), other.clone()];
        let load = act(&mut state, Action::Load(resumed_owner)).unwrap();
        assert!(!state.accept(
            load.sequence,
            Ok(Response {
                target: target(),
                reply: ImportReply::Listed(listed.clone()),
            })
        ));
        assert!(
            state.local_uncertain && !state.can_new(),
            "List does not resolve the original unanswered request"
        );
        let discovery = state.discovery;
        assert!(
            act(
                &mut state,
                Action::Resume {
                    owner: resumed_owner,
                    discovery,
                    operation: other.plan.operation,
                }
            )
            .is_none()
        );
        assert_eq!(
            state.plan.as_ref(),
            Some(&plan),
            "cannot leave unresolved local custody for another record"
        );
        assert!(
            act(
                &mut state,
                Action::Resume {
                    owner: resumed_owner,
                    discovery,
                    operation: plan.operation,
                }
            )
            .is_none()
        );
        assert!(
            state.local_uncertain && !state.can_new(),
            "even resuming the same cached Registered record cannot acknowledge a newer lost Verify"
        );
        let after_cached_resume = state.owner;
        assert_ne!(after_cached_resume, resumed_owner);
        let read = act(&mut state, Action::Status(after_cached_resume)).unwrap();
        assert!(
            !state.accept(
                read.sequence,
                Ok(Response {
                    target: target(),
                    reply: ImportReply::Status(historical.clone()),
                })
            ),
            "terminal metadata acknowledgement creates no registry/provider task"
        );
        assert!(state.can_new());
        assert!(!state.local_uncertain && state.status.as_ref().unwrap().uncertain);
        assert!(
            act(
                &mut state,
                Action::Resume {
                    owner: after_cached_resume,
                    discovery,
                    operation: other.plan.operation,
                }
            )
            .is_none()
        );
        assert_eq!(state.plan.as_ref(), Some(&other.plan));
        assert!(state.can_new());
        let current_owner = state.owner;
        assert_ne!(current_owner, resumed_owner);
        assert!(act(&mut state, Action::New(resumed_owner)).is_none());
        assert_eq!(
            state.plan.as_ref(),
            Some(&other.plan),
            "old form callback stays retired"
        );
        let mut app = context_inspector_test_app(false);
        let registry = app.account_ui.registry.clone();
        app.account_ui.import = state;
        assert!(
            app.account_ui
                .import
                .registered_account(&app.account_ui)
                .is_none(),
            "journal metadata does not create/select an account or authorize Check"
        );
        let check = app.update_account_import(Action::Check(current_owner));
        assert_eq!(check.units(), 0);
        drop(check);
        assert_eq!(app.account_ui.registry, registry);
        let callbacks = click(view(&app).unwrap(), "New import");
        assert!(
            matches!(callbacks.as_slice(), [Message::AccountUi(super::super::Action::Import(Action::New(actual)))] if *actual == current_owner)
        );
        for callback in callbacks {
            let Message::AccountUi(super::super::Action::Import(action)) = callback else {
                panic!("only import action")
            };
            assert!(act(&mut app.account_ui.import, action).is_none());
        }
        assert!(app.account_ui.import.plan.is_none() && app.account_ui.import.pending.is_none());
        assert_eq!(app.account_ui.import.retained, listed);
        assert_eq!(app.account_ui.registry, registry);
    }
}

#[test]
fn lost_verify_existing_profile_preserves_original_intent_and_requires_an_explicit_current_account_check()
 {
    use crate::account::coding_config::{CreateReport, Outcome, Refusal};
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };

    #[derive(Default)]
    struct Texts(Vec<(String, iced::Rectangle)>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
            self.0.push((text.to_owned(), bounds));
        }
    }

    for network_access in [false, true] {
        let mut state = inspected();
        let owner = state.owner;
        act(&mut state, Action::WorkspaceWrite(owner, true));
        act(&mut state, Action::Network(owner, network_access));
        let prepare = act(&mut state, Action::Prepare(owner)).unwrap();
        let plan = state.plan.clone().unwrap();
        assert_eq!(
            plan.profile,
            CodingProfile::WorkspaceWrite { network_access }
        );
        let published = status(&plan, ImportStage::Published);
        let original_receipt = published.receipt.clone().unwrap();
        finish(&mut state, &prepare, published.clone());

        let mut app = context_inspector_test_app(false);
        let previous_id = app.account_ui.add_managed_fixture(Provider::Codex, 7128);
        let previous_account = app
            .account_ui
            .account(&previous_id)
            .unwrap()
            .account
            .clone();
        app.account_ui.registry.set_default(&previous_account);
        let registry_before = app.account_ui.registry.clone();
        app.account_ui.import = state;
        // Only the existing explicit Verify reload is observable here. Its
        // future is never polled and this path is never opened.
        app.account_ui.store = Some(Store::at("/not-opened/import-profile-ui-fixture/registry"));

        let verify = |app: &mut Cagents| {
            let callbacks = click(view(app).unwrap(), "Verify identity");
            assert!(
                matches!(callbacks.as_slice(), [Message::AccountUi(super::super::Action::Import(Action::Verify(actual)))] if *actual == owner)
            );
            let Message::AccountUi(super::super::Action::Import(action)) =
                callbacks.into_iter().next().unwrap()
            else {
                panic!("the rendered control emits only its Verify action")
            };
            let request = act(&mut app.account_ui.import, action).unwrap();
            assert_eq!(
                wire_operation(&request, &target().proof).unwrap(),
                ImportOperation::Verify {
                    operation: plan.operation,
                    destination: target().proof,
                }
            );
            request
        };
        let lost = verify(&mut app);
        let task = app.update_account_import(Action::Finished {
            sequence: lost.sequence,
            result: Err(Failure::Unavailable),
        });
        assert_eq!(task.units(), 0);
        drop(task);
        assert!(app.account_ui.import.local_uncertain);
        assert_eq!(app.account_ui.import.status.as_ref(), Some(&published));
        assert!(!app.account_ui.import.can_new());
        assert!(act(&mut app.account_ui.import, Action::Prepare(owner)).is_none());

        let mut recovered = status(&plan, ImportStage::Registered);
        let account = recovered.registered.clone().unwrap();
        recovered.coding_profile = Some(CreateReport {
            account: account.clone(),
            host: target().proof,
            home: crate::account::readiness::PhysicalIdentity {
                device: original_receipt.home.device,
                inode: original_receipt.home.inode,
            },
            network_access,
            outcome: Outcome::NotCreated(Refusal::ExistingConfiguration),
        });
        // The report must remain bound even when the operation and registration
        // fields look valid. A failed match cannot acknowledge a lost Verify.
        for mismatch in 0..4 {
            let request = verify(&mut app);
            let mut wrong = recovered.clone();
            let report = wrong.coding_profile.as_mut().unwrap();
            match mismatch {
                0 => report.home.inode += 1,
                1 => {
                    report.account = "codex/00000000-0000-4000-8000-000000007128"
                        .parse()
                        .unwrap()
                }
                2 => report.network_access = !network_access,
                _ => report.host = proof("different-report-host"),
            }
            let task = app.update_account_import(Action::Finished {
                sequence: request.sequence,
                result: Ok(Response {
                    target: target(),
                    reply: ImportReply::Status(wrong),
                }),
            });
            assert_eq!(task.units(), 0, "unbound reports cannot reload or check");
            drop(task);
            assert_eq!(app.account_ui.import.plan.as_ref(), Some(&plan));
            assert_eq!(app.account_ui.import.status.as_ref(), Some(&published));
            assert!(app.account_ui.import.local_uncertain && !app.account_ui.import.can_new());
            assert!(app.account_ui.import.pending.is_none());
            assert!(!app.account_ui.check.is_open());
            assert_eq!(app.account_ui.registry, registry_before);
        }

        let request = verify(&mut app);
        let task = app.update_account_import(Action::Finished {
            sequence: request.sequence,
            result: Ok(Response {
                target: target(),
                reply: ImportReply::Status(recovered.clone()),
            }),
        });
        assert!(
            task.units() > 0,
            "a valid explicit Verify retains its registry reload"
        );
        drop(task); // Never run Store/provider work in this UI regression.
        assert_eq!(app.account_ui.import.plan.as_ref(), Some(&plan));
        let accepted = app.account_ui.import.status.as_ref().unwrap();
        assert_eq!(accepted.stage, ImportStage::Registered);
        assert_eq!(accepted.receipt.as_ref(), Some(&original_receipt));
        assert_eq!(accepted.observed_destination, target().proof);
        assert_eq!(accepted.registered.as_ref(), Some(&account));
        assert_eq!(
            accepted.coding_profile.as_ref().unwrap().network_access,
            network_access
        );
        assert_eq!(
            accepted.coding_profile.as_ref().unwrap().outcome,
            Outcome::NotCreated(Refusal::ExistingConfiguration)
        );
        assert!(app.account_ui.import.pending.is_none() && !app.account_ui.import.local_uncertain);
        assert!(
            !app.account_ui.check.is_open(),
            "registration never starts a coding check"
        );
        assert_eq!(app.account_ui.registry, registry_before);

        let mut element = view(&app).unwrap();
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(360.0, 2200.0)),
        );
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
        let preserved = "An existing configuration entry was preserved. Requested settings remain unverified; use Check coding access.";
        let (_, bounds) = texts
            .0
            .iter()
            .find(|(text, _)| text == preserved)
            .expect("the actual narrow import view explains the preserved configuration entry");
        assert!(
            bounds.width > 0.0
                && bounds.height > 0.0
                && bounds.x >= 0.0
                && bounds.x + bounds.width <= 360.0
                && bounds.y + bounds.height <= 2200.0
        );
        assert!(
            !texts
                .0
                .iter()
                .any(|(text, _)| text.starts_with("Separate coding profile created."))
        );
        drop(element);
        assert!(
            app.account_ui
                .import
                .registered_account(&app.account_ui)
                .is_none(),
            "a bound report is metadata, not a current registered account"
        );
        assert!(click(view(&app).unwrap(), "Check coding access").is_empty());
        let task = app.update_account_import(Action::Check(owner));
        assert_eq!(task.units(), 0);
        drop(task);
        assert!(!app.account_ui.check.is_open());

        // Supply a real current UI registry row, using the existing in-memory
        // fixture path only. No Store, home, configuration or provider is used.
        let mut registry = app.account_ui.registry.clone();
        registry
            .register(
                crate::account::registry::Registered {
                    key: plan.operation,
                    provider: Provider::Codex,
                    linked_home: None,
                    label: None,
                    email: Some(plan.expected_email.clone()),
                    organization: None,
                    added_at: 0,
                },
                &[],
            )
            .unwrap();
        app.account_ui.set_registry(registry);
        assert_eq!(
            app.account_ui.import.registered_account(&app.account_ui),
            Some(account.clone())
        );
        assert!(app.account_ui.registry.is_default(&previous_account));
        let registry = app.account_ui.registry.clone();
        let callbacks = click(view(&app).unwrap(), "Check coding access");
        assert!(
            matches!(callbacks.as_slice(), [Message::AccountUi(super::super::Action::Import(Action::Check(actual)))] if *actual == owner)
        );
        let Message::AccountUi(super::super::Action::Import(action)) =
            callbacks.into_iter().next().unwrap()
        else {
            panic!("explicit check only")
        };
        let task = app.update_account_import(action);
        assert!(task.units() > 0);
        drop(task); // Explicit Check opens its form; never poll its preparation.
        assert!(app.account_ui.check.is_open() && !app.account_ui.import.visible);
        assert_eq!(app.account_ui.import.plan.as_ref(), Some(&plan));
        assert_eq!(app.account_ui.import.status.as_ref(), Some(&recovered));
        assert_eq!(app.account_ui.registry, registry);
    }
}

#[test]
fn an_unsent_prepare_after_destination_change_restores_explicit_inspection_without_resending() {
    let mut state = inspected();
    let old_owner = state.owner;
    act(&mut state, Action::WorkspaceWrite(old_owner, true));
    act(&mut state, Action::Network(old_owner, true));
    let input = (
        state.path.clone(),
        state.email.clone(),
        state.copy,
        state.session,
        state.write,
        state.network,
        state.profile.clone(),
    );
    let prepare = act(&mut state, Action::Prepare(old_owner)).unwrap();
    let old_plan = state.plan.clone().unwrap();
    assert_eq!(
        wire_operation(&prepare, &target().proof).unwrap(),
        ImportOperation::Prepare {
            plan: old_plan.clone()
        },
        "an unchanged destination still produces the exact original dispatch shape"
    );
    let restarted = proof("restarted-before-prepare");
    let not_sent = wire_operation(&prepare, &restarted).unwrap_err();
    assert!(matches!(not_sent, Failure::PrepareNotSent));
    assert!(
        act(
            &mut state,
            Action::Finished {
                sequence: prepare.sequence + 1,
                result: Err(not_sent),
            }
        )
        .is_none()
    );
    assert_eq!(state.plan.as_ref(), Some(&old_plan));
    assert_eq!(state.pending.as_ref().unwrap().sequence, prepare.sequence);
    assert!(state.local_uncertain && !state.editable());

    let mut app = context_inspector_test_app(false);
    app.account_ui.import = state;
    let task = app.update_account_import(Action::Finished {
        sequence: prepare.sequence,
        result: Err(not_sent),
    });
    assert_eq!(
        task.units(),
        0,
        "a proven unsent result cannot reload the registry or send another import"
    );
    drop(task);
    let state = &app.account_ui.import;
    assert!(state.plan.is_none() && state.pending.is_none() && state.status.is_none());
    assert!(state.inspection.is_none() && !state.local_uncertain);
    assert!(state.editable() && !state.can_prepare());
    assert_ne!(state.owner, old_owner);
    assert_eq!(
        (
            state.path.clone(),
            state.email.clone(),
            state.copy,
            state.session,
            state.write,
            state.network,
            state.profile.clone()
        ),
        input
    );
    assert_eq!(state.problem, Some(Failure::PrepareNotSent.description()));
    let owner = state.owner;
    assert!(act(&mut app.account_ui.import, Action::Prepare(old_owner)).is_none());
    assert!(
        act(&mut app.account_ui.import, Action::Prepare(owner)).is_none(),
        "no preparation without a fresh explicit inspection"
    );

    let messages = click(view(&app).unwrap(), "Inspect file");
    assert_eq!(messages.len(), 1, "one explicit inspection callback only");
    let inspect_action = match messages
        .into_iter()
        .next()
        .expect("enabled explicit Inspect")
    {
        Message::AccountUi(super::super::Action::Import(action)) => action,
        Message::UtilityPaneUi(message) => match *message {
            Message::AccountUi(super::super::Action::Import(action)) => action,
            _ => panic!("expected the import inspector"),
        },
        _ => panic!("expected the import inspector"),
    };
    assert!(matches!(&inspect_action, Action::Inspect(n) if *n == owner));
    let inspect = act(&mut app.account_ui.import, inspect_action).unwrap();
    assert!(matches!(&inspect.kind, RequestKind::Inspect(_)));
    assert!(
        act(
            &mut app.account_ui.import,
            Action::Finished {
                sequence: inspect.sequence,
                result: Ok(Response {
                    target: Target {
                        proof: restarted.clone(),
                        ..target()
                    },
                    reply: ImportReply::Inspected(ImportInspection {
                        destination: restarted.clone(),
                        observation: observation(),
                    }),
                }),
            }
        )
        .is_none()
    );
    assert!(
        app.account_ui.import.pending.is_none() && app.account_ui.import.plan.is_none(),
        "inspection alone never starts preparation"
    );
    let second = act(&mut app.account_ui.import, Action::Prepare(owner)).unwrap();
    let new_plan = app.account_ui.import.plan.clone().unwrap();
    assert_ne!(new_plan.operation, old_plan.operation);
    assert_eq!(new_plan.destination, restarted);
    assert_eq!(new_plan.expected_email, old_plan.expected_email);
    assert_eq!(new_plan.consent, old_plan.consent);
    assert_eq!(new_plan.profile, old_plan.profile);
    assert_eq!(
        wire_operation(&second, &restarted).unwrap(),
        ImportOperation::Prepare {
            plan: new_plan.clone()
        }
    );
    assert!(
        act(
            &mut app.account_ui.import,
            Action::Finished {
                sequence: prepare.sequence,
                result: Err(not_sent),
            }
        )
        .is_none()
    );
    assert_eq!(app.account_ui.import.plan.as_ref(), Some(&new_plan));
    assert_eq!(
        app.account_ui.import.pending.as_ref().unwrap().sequence,
        second.sequence
    );
    assert!(app.account_ui.import.local_uncertain && !app.account_ui.import.editable());
}

#[test]
fn only_a_matching_fresh_prepare_not_sent_result_can_release_local_custody() {
    for failure in [
        Failure::Unavailable,
        Failure::Changed,
        Failure::Invalid,
        Failure::Journal,
    ] {
        let (mut state, prepare) = prepared();
        let plan = state.plan.clone().unwrap();
        let owner = state.owner;
        act(
            &mut state,
            Action::Finished {
                sequence: prepare.sequence,
                result: Err(failure),
            },
        );
        assert_eq!(state.plan.as_ref(), Some(&plan));
        assert!(state.pending.is_none() && state.local_uncertain);
        assert!(!state.editable() && !state.can_new());
        assert!(act(&mut state, Action::Inspect(owner)).is_none());
        assert!(act(&mut state, Action::New(owner)).is_none());
    }
    for verify in [false, true] {
        let (mut state, prepare) = prepared();
        let plan = state.plan.clone().unwrap();
        let owner = state.owner;
        if verify {
            let mut published = status(&plan, ImportStage::Published);
            published.uncertain = true;
            finish(&mut state, &prepare, published);
        } else {
            act(
                &mut state,
                Action::Finished {
                    sequence: prepare.sequence,
                    result: Err(Failure::Unavailable),
                },
            );
        }
        let followup = act(
            &mut state,
            if verify {
                Action::Verify(owner)
            } else {
                Action::Status(owner)
            },
        )
        .unwrap();
        // The typed local outcome has no production path from Status/Verify;
        // even a mismatched internal result cannot unlock those originals.
        act(
            &mut state,
            Action::Finished {
                sequence: followup.sequence,
                result: Err(Failure::PrepareNotSent),
            },
        );
        assert_eq!(state.plan.as_ref(), Some(&plan));
        assert!(state.pending.is_none() && state.local_uncertain);
        assert!(!state.editable() && !state.can_new());
        assert_eq!(state.problem, Some(Failure::Invalid.description()));
        assert_eq!(state.owner, owner);
    }
}

#[test]
fn import_requests_retain_scope_through_reconnect_and_reject_rebased_replies() {
    use crate::agent_execution::Authority;
    let scope = Authority::session("fixture-original-scope".into());
    let mut state = State::with_authority(scope.clone());
    act(&mut state, Action::Open);
    let owner = state.owner;
    for action in [
        Action::Path(owner, "/fixture/auth.json".into()),
        Action::Email(owner, "fixture@example.test".into()),
        Action::CopyConsent(owner, true),
        Action::SessionConsent(owner, true),
    ] {
        act(&mut state, action);
    }
    let inspect = act(&mut state, Action::Inspect(owner)).unwrap();
    assert_eq!(inspect.authority, scope);
    let scoped_target = Target {
        authority: scope.clone(),
        ..target()
    };
    act(
        &mut state,
        Action::Finished {
            sequence: inspect.sequence,
            result: Ok(Response {
                target: scoped_target.clone(),
                reply: ImportReply::Inspected(ImportInspection {
                    destination: scoped_target.proof.clone(),
                    observation: observation(),
                }),
            }),
        },
    );
    let prepare = act(&mut state, Action::Prepare(owner)).unwrap();
    assert_eq!(prepare.authority, scope);
    let plan = state.plan.clone().unwrap();
    assert!(wire_operation(&prepare, &scoped_target.proof).is_ok());
    let mut rebased = prepare.clone();
    rebased.authority = Authority::default();
    assert!(matches!(
        wire_operation(&rebased, &scoped_target.proof),
        Err(Failure::PrepareNotSent)
    ));
    act(
        &mut state,
        Action::Finished {
            sequence: prepare.sequence,
            result: Err(Failure::Unavailable),
        },
    );
    act(&mut state, Action::Close(owner));
    act(&mut state, Action::Open);
    let owner = state.owner;
    let status_request = act(&mut state, Action::Status(owner)).unwrap();
    assert_eq!(status_request.authority, scope);
    act(
        &mut state,
        Action::Finished {
            sequence: status_request.sequence,
            result: Ok(Response {
                target: target(),
                reply: ImportReply::Status(status(&plan, ImportStage::Published)),
            }),
        },
    );
    assert!(
        state.status.is_none(),
        "a User-scoped reply cannot settle the original request"
    );
    assert_eq!(state.plan.as_ref(), Some(&plan));
    let retry_status = act(&mut state, Action::Status(owner)).unwrap();
    assert_eq!(retry_status.authority, scope);
    act(
        &mut state,
        Action::Finished {
            sequence: retry_status.sequence,
            result: Ok(Response {
                target: scoped_target,
                reply: ImportReply::Status(status(&plan, ImportStage::Published)),
            }),
        },
    );
    let verify = act(&mut state, Action::Verify(owner)).unwrap();
    assert_eq!(verify.authority, scope);
    assert!(
        matches!(wire_operation(&verify, &proof("reconnected")), Ok(ImportOperation::Verify { operation, .. }) if operation == plan.operation)
    );
    act(
        &mut state,
        Action::Finished {
            sequence: verify.sequence,
            result: Ok(Response {
                target: Target {
                    authority: scope.clone(),
                    proof: proof("reconnected"),
                    ..target()
                },
                reply: ImportReply::Status({
                    let mut value = status(&plan, ImportStage::Published);
                    value.observed_destination = proof("reconnected");
                    value
                }),
            }),
        },
    );
    assert_eq!(state.status.as_ref().unwrap().stage, ImportStage::Published);
    assert_eq!(state.authority, scope);
}
