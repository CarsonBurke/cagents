use super::*;
use crate::terminal::ipc::TerminalDescriptor;

fn fixture() -> (Cagents, CardId, TerminalSession) {
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let card = app.durable.cards[0].id;
    let CardKind::Terminal(source) = app.durable.cards[0].kind.clone() else {
        unreachable!()
    };
    app.cards.insert(card, CardRuntime::Loading);
    (app, card, source)
}
fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("saved-terminal-host"),
        boot_id: crate::model::BootId::new("saved-terminal-boot"),
        host_incarnation: crate::model::HostIncarnation::new("saved-terminal-incarnation"),
    }
}
fn descriptor(source: TerminalSession) -> TerminalDescriptor {
    let mut descriptor = TerminalDescriptor::relaunchable(
        source.id,
        source.launch.clone(),
        source.transcript_path.clone(),
    );
    descriptor.session = source;
    descriptor
}
fn exact_live(source: &TerminalSession) -> TerminalSession {
    let mut live = source.clone();
    let proof = proof();
    live.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    live
}
fn pending(app: &mut Cagents, card: CardId, source: &TerminalSession) -> uuid::Uuid {
    assert!(app.prepare_saved_terminal(card, source));
    let entry = app.saved_terminals.entries.get_mut(&card).unwrap();
    entry.authority = crate::agent_execution::Authority::default();
    entry.socket = Ok(Some(
        crate::terminal::ipc::daemon_socket_path().expect("fixture socket address"),
    ));
    let request = entry.request;
    app.begin_terminal_start(card);
    app.saved_terminals.outstanding.insert(request, card);
    request
}
fn complete(
    card: CardId,
    source: TerminalSession,
    request: uuid::Uuid,
    result: Result<Option<StartedTerminal>, String>,
) -> Action {
    Action::Inspected {
        card,
        source,
        request,
        socket: Some(crate::terminal::ipc::daemon_socket_path().expect("fixture socket address")),
        relaunch: false,
        directory: Directory::Available,
        result,
    }
}

#[test]
fn exact_issued_live_descriptor_wins_over_missing_saved_directory() {
    let (_, _, mut source) = fixture();
    source.launch.working_directory = PathBuf::from("/absent/saved-terminal-fixture");
    if let TerminalContinuity::RelaunchAvailable { recipe, .. } = &mut source.continuity {
        *recipe = source.launch.clone();
    }
    let live = exact_live(&source);
    assert_eq!(
        attachment(&source, &[descriptor(live.clone())], &proof()).unwrap(),
        Some(live)
    );
    assert!(source.continuity.live_proof().is_none());
    assert!(!terminal_ended(&source));
}

#[test]
fn matching_id_cannot_replace_source_recipe_or_resume_evidence() {
    let (_, _, source) = fixture();
    for changed in [false, true] {
        let mut live = exact_live(&source);
        if changed {
            live.resume_hint = Some("different-source".into());
        } else {
            live.launch.working_directory = PathBuf::from("/different-source");
        }
        assert!(attachment(&source, &[descriptor(live)], &proof()).is_err());
    }
    let mut live = exact_live(&source);
    if let TerminalContinuity::DetachedSameBoot {
        host_incarnation, ..
    } = &mut live.continuity
    {
        *host_incarnation = crate::model::HostIncarnation::new("wrong-host");
    }
    assert!(attachment(&source, &[descriptor(live)], &proof()).is_err());
}

#[test]
fn proofless_source_does_not_guess_a_descendant_or_assume_incomplete_host_absence() {
    let (_, _, source) = fixture();
    let mut successor = exact_live(&source);
    successor.id = TerminalId::new();
    successor.recovery_origin = Some(source.id);
    assert!(attachment(&source, &[descriptor(successor)], &proof()).is_err());
    let mut incomplete = proof();
    incomplete.boot_id = crate::model::BootId::new("");
    assert!(attachment(&source, &[], &incomplete).is_err());
    assert_eq!(attachment(&source, &[], &proof()).unwrap(), None);
}

#[test]
fn shared_creation_and_existing_worker_are_not_saved_dormant_candidates() {
    let (mut app, card, source) = fixture();
    Arc::make_mut(&mut app.durable)
        .pending_terminal_creations
        .insert(card);
    assert!(!app.prepare_saved_terminal(card, &source));
    Arc::make_mut(&mut app.durable)
        .pending_terminal_creations
        .remove(&card);
    app.shared_views.attached.insert(card);
    assert!(!app.prepare_saved_terminal(card, &source));
    app.shared_views.attached.remove(&card);
    app.begin_terminal_start(card);
    assert!(!app.prepare_saved_terminal(card, &source));
    drop(app.finish_terminal_start(card));
}

#[test]
fn pending_then_exact_absence_keeps_the_original_descriptor_and_clears_start_status() {
    let (mut app, card, source) = fixture();
    let request = pending(&mut app, card, &source);
    assert!(!app.saved_terminal_ready(card));
    assert_eq!(
        app.saved_terminal_status(card),
        Some(("Checking saved terminal", true))
    );
    assert!(app.saved_terminal_view(card).is_some());
    drop(app.update_saved_terminal(complete(card, source.clone(), request, Ok(None))));
    assert!(app.saved_terminal_ready(card));
    assert_eq!(app.saved_terminal_status(card), None);
    assert_eq!(
        app.inspector_runtime_status(card),
        InspectorRuntimeStatus::Unavailable
    );
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(source.clone())
    );
    assert!(!terminal_ended(&source));
    assert!(!app.terminal_start_in_flight(card));
}

#[test]
fn inventory_failure_is_actionable_and_never_dormant_ready() {
    let (mut app, card, source) = fixture();
    let request = pending(&mut app, card, &source);
    drop(app.update_saved_terminal(complete(
        card,
        source,
        request,
        Err("startup custody unresolved".into()),
    )));
    assert!(!app.saved_terminal_ready(card));
    assert_eq!(
        app.saved_terminal_status(card),
        Some(("Reconnect unavailable", false))
    );
    assert!(app.saved_terminal_view(card).is_some());
    assert!(!app.terminal_start_in_flight(card));
}

#[test]
fn stale_and_duplicate_callbacks_cannot_install_dormancy_or_consume_a_new_worker() {
    let (mut app, card, source) = fixture();
    let request = pending(&mut app, card, &source);
    let mut replacement = source.clone();
    replacement.resume_hint = Some("new saved source".into());
    app.card_state_mut(card).unwrap().kind = CardKind::Terminal(replacement.clone());
    let stale = complete(card, source, request, Ok(None));
    drop(app.update_saved_terminal(stale.clone()));
    assert!(app.saved_terminal_current(card).is_none());
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(replacement)
    );
    app.begin_terminal_start(card);
    drop(app.update_saved_terminal(stale));
    assert!(
        app.terminal_start_in_flight(card),
        "duplicate must not spend a newer start"
    );
    drop(app.finish_terminal_start(card));
}

#[test]
fn saved_user_intents_are_blocked_by_modals_and_update_but_results_can_settle() {
    let (_, card, source) = fixture();
    let request = uuid::Uuid::new_v4();
    for command in [
        Command::Reconnect,
        Command::Relaunch,
        Command::CopyDetails,
        Command::Transcript,
        Command::Folder,
        Command::NewTerminal,
        Command::Close,
    ] {
        let message = Message::SavedTerminal(Action::User {
            card,
            request,
            command,
        });
        assert!(user_workspace_message_blocked_while_modal(&message));
        assert!(user_message_blocked_during_update(&message));
    }
    let result = Message::SavedTerminal(complete(card, source, request, Ok(None)));
    assert!(!user_workspace_message_blocked_while_modal(&result));
    assert!(!user_message_blocked_during_update(&result));
}

#[test]
fn a_replaced_presentation_cannot_launch_from_a_queued_old_button() {
    let (mut app, card, source) = fixture();
    assert!(app.prepare_saved_terminal(card, &source));
    let request = app.saved_terminals.entries[&card].request;
    app.saved_terminals.entries.get_mut(&card).unwrap().request = uuid::Uuid::new_v4();
    for command in [
        Command::Reconnect,
        Command::Relaunch,
        Command::Folder,
        Command::NewTerminal,
        Command::Close,
    ] {
        drop(app.update_saved_terminal(Action::User {
            card,
            request,
            command,
        }));
        assert!(!app.terminal_start_in_flight(card));
        assert!(app.card_state(card).unwrap().existence.is_open());
    }
}

#[test]
fn old_proof_saved_panes_use_the_same_non_spawning_admission_marker() {
    let (mut app, card, source) = fixture();
    let source = exact_live(&source);
    app.card_state_mut(card).unwrap().kind = CardKind::Terminal(source.clone());
    assert!(app.prepare_saved_terminal(card, &source));
    let entry = app.saved_terminal_current(card).unwrap();
    assert_eq!(entry.source, source);
    assert!(!app.saved_terminal_ready(card));
    assert!(!matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
}

#[test]
fn a_window_of_saved_panes_has_one_active_inspection_lane() {
    let (mut app, first, source) = fixture();
    let original_card = app.card_state(first).unwrap().clone();
    let mut cards = vec![(first, source)];
    for _ in 0..5 {
        let mut card = original_card.clone();
        card.id = CardId::new();
        let CardKind::Terminal(source) = &mut card.kind else {
            unreachable!()
        };
        source.id = TerminalId::new();
        cards.push((card.id, source.clone()));
        Arc::make_mut(&mut app.durable).cards.push(card);
    }
    for (card, source) in &cards {
        assert!(app.prepare_saved_terminal(*card, source));
        let entry = app.saved_terminals.entries.get_mut(card).unwrap();
        entry.authority = crate::agent_execution::Authority::default();
        entry.socket = Ok(Some(
            crate::terminal::ipc::daemon_socket_path().expect("fixture socket address"),
        ));
        // This is a task-construction test; no task is polled and no socket,
        // filesystem probe, daemon or provider is opened.
        drop(app.inspect_saved_terminal(*card));
    }
    assert_eq!(app.terminal_starts_in_flight(), 1);
    assert_eq!(app.saved_terminals.outstanding.len(), 1);
    assert_eq!(app.saved_terminals.queue.len(), cards.len() - 1);
    let (card, source) = cards[0].clone();
    let request = app.saved_terminals.entries[&card].request;
    drop(app.update_saved_terminal(complete(card, source, request, Ok(None))));
    assert!(app.saved_terminal_ready(card));
    assert_eq!(
        app.terminal_starts_in_flight(),
        1,
        "only the next queued inspection is admitted"
    );
    assert_eq!(app.saved_terminals.outstanding.len(), 1);
    assert_eq!(app.saved_terminals.queue.len(), cards.len() - 2);
}

#[test]
fn directory_evidence_is_presentation_and_does_not_change_the_saved_recipe() {
    let (mut app, card, source) = fixture();
    let request = pending(&mut app, card, &source);
    // Submit the actual same owned completion with explicit missing-folder data.
    let action = Action::Inspected {
        card,
        source: source.clone(),
        request,
        socket: Some(crate::terminal::ipc::daemon_socket_path().expect("fixture socket address")),
        relaunch: false,
        directory: Directory::Missing,
        result: Ok(None),
    };
    drop(app.update_saved_terminal(action));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(source)
    );
    assert!(app.saved_terminal_ready(card));
    assert!(app.saved_terminal_view(card).is_some());
}

#[test]
fn diagnostics_cannot_borrow_user_scope_or_a_later_endpoint() {
    let (mut app, card, source) = fixture();
    let request = pending(&mut app, card, &source);
    assert!(!app.saved_terminal_diagnostics_allowed(card));
    drop(app.update_saved_terminal(complete(card, source, request, Ok(None))));
    assert!(app.saved_terminal_diagnostics_allowed(card));
    let endpoint = app.saved_terminals.entries[&card].socket.clone();
    app.saved_terminals
        .entries
        .get_mut(&card)
        .unwrap()
        .authority =
        crate::agent_execution::Authority::session("synthetic-scoped-capability".into());
    assert!(!app.saved_terminal_diagnostics_allowed(card));
    let entry = app.saved_terminals.entries.get_mut(&card).unwrap();
    entry.authority = crate::agent_execution::Authority::default();
    entry.socket = Ok(Some(PathBuf::from(
        "/different-selected-endpoint/control.sock",
    )));
    assert!(!app.saved_terminal_diagnostics_allowed(card));
    app.saved_terminals.entries.get_mut(&card).unwrap().socket = endpoint;
    app.begin_terminal_start(card);
    assert!(!app.saved_terminal_diagnostics_allowed(card));
    drop(app.finish_terminal_start(card));
}

#[test]
fn a_missing_or_unverified_directory_has_no_enabled_relaunch_callback() {
    let (mut app, card, source) = fixture();
    let request = pending(&mut app, card, &source);
    drop(app.update_saved_terminal(complete(card, source.clone(), request, Ok(None))));
    for directory in [Directory::Missing, Directory::Unknown] {
        app.saved_terminals
            .entries
            .get_mut(&card)
            .unwrap()
            .directory = directory;
        drop(app.update_saved_terminal(Action::User {
            card,
            request,
            command: Command::Relaunch,
        }));
        assert!(
            !app.terminal_start_in_flight(card),
            "even an old queued callback must not start inspection or relaunch"
        );
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Terminal(source.clone())
        );
        assert!(app.saved_terminal_view(card).is_some());
    }
}

#[test]
fn remote_user_keeps_its_existing_route_but_scoped_callers_cannot_fall_back_to_user() {
    let user = crate::agent_execution::Authority::default();
    let scoped = crate::agent_execution::Authority::session("synthetic-scoped-capability".into());
    assert!(owns_context(false, &user));
    assert!(
        !owns_context(true, &user),
        "ordinary remote users keep the existing device route"
    );
    assert!(owns_context(false, &scoped));
    assert!(
        owns_context(true, &scoped),
        "inherited remote scope must be captured and refused before bridge resolution"
    );
}

#[test]
fn retained_startup_blocks_absence_but_keeps_exact_live_attachments_inspectable() {
    let (_, _, source) = fixture();
    let live = exact_live(&source);
    let result = inspect_inventory(&source, &[descriptor(live.clone())], &proof(), || {
        panic!("an exact available attachment must not request general startup readiness")
    });
    assert_eq!(result.unwrap(), Some(live));
    assert!(
        inspect_inventory(&source, &[], &proof(), || Err(
            "retained startup ownership".into()
        ))
        .is_err()
    );
    let mut conflicting = exact_live(&source);
    conflicting.launch.working_directory = PathBuf::from("/different-source");
    assert!(
        inspect_inventory(&source, &[descriptor(conflicting)], &proof(), || {
            panic!("a source conflict cannot be converted into admitted absence")
        })
        .is_err()
    );
}

#[test]
fn an_issued_completed_descriptor_is_readable_without_inventing_an_exit() {
    let (_, _, source) = fixture();
    let result = inspect_inventory(&source, &[descriptor(source.clone())], &proof(), || {
        panic!("the exact completed descriptor still permits an inspection attachment")
    })
    .unwrap()
    .unwrap();
    assert_eq!(result, source);
    assert!(matches!(
        result.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: None,
            ..
        }
    ));
    assert!(
        !terminal_ended(&result),
        "readable completion evidence does not manufacture an exit record"
    );
}

#[test]
fn scoped_saved_controls_cannot_dispatch_user_commands_but_user_recovery_remains_available() {
    let (mut app, card, source) = fixture();
    let request = pending(&mut app, card, &source);
    drop(app.update_saved_terminal(complete(card, source.clone(), request, Ok(None))));
    app.saved_terminals
        .entries
        .get_mut(&card)
        .unwrap()
        .authority =
        crate::agent_execution::Authority::session("synthetic-scoped-capability".into());
    let original = app.durable.cards.clone();
    for command in [
        Command::Reconnect,
        Command::Relaunch,
        Command::Transcript,
        Command::Folder,
        Command::NewTerminal,
        Command::Close,
    ] {
        assert!(!allows_command(
            &app.saved_terminals.entries[&card].authority,
            command
        ));
        let task = app.update_saved_terminal(Action::User {
            card,
            request,
            command,
        });
        assert!(
            iced::runtime::task::into_stream(task).is_none(),
            "refused controls cannot enqueue a picker, bridge, launch, close or inspection"
        );
        assert_eq!(app.durable.cards, original);
        assert!(!app.terminal_start_in_flight(card));
    }
    assert!(allows_command(
        &app.saved_terminals.entries[&card].authority,
        Command::CopyDetails
    ));
    app.saved_terminals
        .entries
        .get_mut(&card)
        .unwrap()
        .authority = crate::agent_execution::Authority::default();
    for command in [
        Command::Reconnect,
        Command::Relaunch,
        Command::Transcript,
        Command::Folder,
        Command::NewTerminal,
        Command::Close,
    ] {
        assert!(allows_command(
            &app.saved_terminals.entries[&card].authority,
            command
        ));
    }
    // Construct, but never poll, the ordinary User reinspection task.
    let task = app.update_saved_terminal(Action::User {
        card,
        request,
        command: Command::Relaunch,
    });
    assert!(iced::runtime::task::into_stream(task).is_some());
    assert!(app.terminal_start_in_flight(card));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(source)
    );
}
