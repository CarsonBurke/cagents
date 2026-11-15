use super::*;

const DETAIL: &str = "Upgrade restore retained original ownership: terminal dimensions or scrollback are outside supported bounds";

/// An opened, handshaking window whose saved terminals are all still
/// attaching, after the gate's first check has recorded them.
fn waiting(panes: usize) -> (Cagents, Vec<CardId>) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut cards = Vec::new();
    for _ in 0..panes {
        let card = crate::app::tests::terminal_card_state();
        app.cards.insert(card.id, CardRuntime::Loading);
        cards.push(card.id);
        Arc::make_mut(&mut app.durable).cards.push(card);
    }
    app.startup_gate = State::fixture_opened();
    app.startup_gate.budget = Some(launcher_budget(panes));
    // These pure gate fixtures record Pending without reserving an unpolled
    // inspection worker. Producer occupancy is exercised separately below.
    app.startup_gate.inspected = Some(Instant::now());
    drop(app.verify_startup_gate());
    app.startup_gate.inspected = None;
    assert_eq!(
        app.startup_gate
            .pending
            .as_ref()
            .map(|pending| &pending.panes),
        Some(&cards)
    );
    (app, cards)
}

fn fixture_proof() -> crate::model::HostSessionProof {
    crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    }
}

/// Complete one issued observation without starting a daemon or executing Tasks.
fn inspection(app: &mut Cagents, state: Result<StartupState, String>) -> Inspection {
    let pending = app.startup_pending().unwrap();
    let owners = app.startup_connected_owners();
    let gate = &mut app.startup_gate;
    let ticket = gate.next_inspection.checked_add(1).unwrap();
    gate.next_inspection = ticket;
    let context = InspectionContext {
        ticket,
        opened: gate.opened.unwrap(),
        pending,
        owners,
    };
    gate.outstanding = Some(ticket);
    gate.inspecting = Some(context.clone());
    Inspection {
        context,
        outcome: state.map(|state| Observation {
            socket: gate.fixture_endpoint.clone().unwrap(),
            proof: fixture_proof(),
            state,
        }),
    }
}

fn deliver(app: &mut Cagents, state: Result<StartupState, String>) -> Task<Message> {
    // Late unsolicited answers must remain late; do not create a new active
    // request after saving or reporting just to deliver a fixture message.
    let context = app
        .startup_gate
        .inspecting
        .clone()
        .unwrap_or_else(|| InspectionContext {
            ticket: app.startup_gate.next_inspection,
            opened: app.startup_gate.opened.unwrap(),
            pending: app.startup_gate.pending.clone(),
            owners: app.startup_connected_owners(),
        });
    let inspection = if app.startup_gate.reported || app.startup_gate.saving {
        Inspection {
            context,
            outcome: state.map(|state| Observation {
                socket: app.startup_gate.fixture_endpoint.clone().unwrap(),
                proof: fixture_proof(),
                state,
            }),
        }
    } else {
        inspection(app, state)
    };
    app.startup_state_inspected(inspection)
}

/// Moves the gate's clock back instead of sleeping through its bounds.
fn elapse(app: &mut Cagents, by: Duration) {
    let gate = &mut app.startup_gate;
    let back = |at: Instant| at.checked_sub(by).unwrap();
    gate.opened = gate.opened.map(back);
    gate.progressed = gate.progressed.map(back);
    gate.settled = gate.settled.map(back);
    gate.asked = gate.asked.map(back);
    gate.inspected = gate.inspected.map(back);
    if let Some(context) = &mut gate.inspecting {
        context.opened = back(context.opened);
    }
    if let Some(observed) = &mut gate.daemon {
        observed.context.opened = back(observed.context.opened);
    }
}

fn reported(app: &Cagents) -> Option<&str> {
    app.startup_gate
        .reported
        .then(|| app.status.as_ref().unwrap().message.as_str())
}

fn reported_details(app: &Cagents) -> Option<&str> {
    app.startup_gate
        .reported
        .then(|| app.status.as_ref().unwrap().details.as_deref())
        .flatten()
}

/// The notice is read by a person; only the launcher answer carries the
/// handshake's machine prefix.
fn assert_readable(notice: &str) {
    assert!(
        !notice.contains(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX.trim_end()),
        "{notice}"
    );
    assert!(!notice.contains("recovery controls"), "{notice}");
}

#[test]
fn retained_daemon_recovery_reports_its_reason_at_once_and_keeps_the_panes() {
    let (mut app, cards) = waiting(3);
    drop(deliver(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 2 }),
    ));
    drop(app.verify_startup_gate());
    assert_eq!(
        reported(&app),
        None,
        "a restore in progress is not a failure"
    );

    drop(deliver(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    ));
    let reason = reported(&app).expect("retained recovery must answer the launcher");
    assert_readable(reason);
    assert!(!reason.contains(DETAIL), "{reason}");
    assert_eq!(reported_details(&app), Some(DETAIL));
    // Launchers keep the window open only for this error form.
    let answer = retained_reason(DETAIL);
    assert!(
        answer.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX),
        "{answer}"
    );
    assert!(answer.contains(DETAIL), "{answer}");
    assert!(answer.contains("this window remains available"), "{answer}");
    for card in cards {
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    }
    // The gate answers once; later checks and answers change nothing.
    let reason = reason.to_owned();
    drop(app.verify_startup_gate());
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    assert_eq!(reported(&app), Some(reason.as_str()));
}

#[test]
fn retained_reason_fits_one_launcher_handshake_line() {
    let detail = "Restore failed: ünïcödé\nsecond line ".repeat(500);
    let line = format!(
        "{}{}",
        instance::UI_LAUNCH_ERROR_PREFIX,
        retained_reason(&detail)
    );
    // The launcher refuses a response longer than 4 KiB, which would close
    // the recovery window it should keep.
    assert!(line.len() < 4096, "{}", line.len());
    assert!(!line.contains('\n'));
    assert!(line.contains("Restore failed: ünïcödé second line"));
}

#[test]
fn panes_without_progress_fail_before_the_inventory_budget_once_the_daemon_is_ready() {
    // Three panes give a 120 second inventory budget.
    let (mut app, cards) = waiting(3);
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    elapse(&mut app, NO_PROGRESS - Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    elapse(&mut app, Duration::from_secs(2));
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("a stalled gate must answer the launcher");
    assert_readable(reason);
    assert!(reason.starts_with("3 saved panes"), "{reason}");
    assert!(reason.contains("made no progress"), "{reason}");
    let answer = app.stalled_reason(&Pending {
        admission: false,
        panes: cards.clone(),
    });
    assert!(
        answer.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX),
        "{answer}"
    );
    // Panes are named as their tabs are, not by internal identifiers.
    assert!(reason.contains("\u{201c}Terminal\u{201d}"), "{reason}");
    assert!(!reason.contains(&cards[0].to_string()), "{reason}");
}

#[test]
fn settling_panes_or_a_restoring_daemon_keep_the_gate_waiting() {
    // Ten panes give a 330 second inventory budget.
    let (mut app, cards) = waiting(10);
    drop(deliver(&mut app, Ok(StartupState::Ready)));

    // One pane settling is progress, even though it failed.
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    app.cards.insert(
        cards[0],
        CardRuntime::Failed("Fixture attach failed".into()),
    );
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    // The panes cannot move while the daemon is still restoring...
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    drop(deliver(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 3 }),
    ));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);
    // ...and its finishing is progress in its own right.
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    // A daemon that cannot answer is not evidence of a stall.
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    drop(deliver(
        &mut app,
        Err("daemon socket connection deadline elapsed".into()),
    ));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);
}

#[test]
fn the_inventory_budget_still_caps_a_daemon_that_keeps_restoring() {
    let (mut app, _) = waiting(1);
    drop(deliver(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 1 }),
    ));
    elapse(&mut app, launcher_budget(1) - Duration::from_secs(1));
    drop(deliver(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 1 }),
    ));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    elapse(&mut app, Duration::from_secs(1));
    drop(deliver(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 1 }),
    ));
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("the absolute cap still applies");
    assert_readable(reason);
    assert!(
        reason.starts_with("1 saved pane did not reconnect in time: \u{201c}Terminal\u{201d}"),
        "{reason}"
    );
}

#[test]
fn inspection_refuses_remote_before_resolving_an_ordinary_starting_bridge() {
    let called = std::cell::Cell::new(0);
    let remote = inspect_existing_daemon(true, || {
        called.set(called.get() + 1);
        panic!("an ordinary selected bridge must not be resolved by inspection");
    });
    assert!(remote.unwrap_err().contains("unavailable"));
    assert_eq!(called.get(), 0);
    let local = inspect_existing_daemon(false, || {
        called.set(called.get() + 1);
        Err("private local connection result".into())
    });
    assert_eq!(local.unwrap_err(), "private local connection result");
    assert_eq!(called.get(), 1, "the guard must preserve local inspection");
}

#[test]
fn old_inspection_cannot_clear_a_new_ticket_or_change_its_progress() {
    let (mut app, _) = waiting(3);
    let old = inspection(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    );
    let current = inspection(&mut app, Ok(StartupState::Ready));
    let active = app.startup_gate.inspecting.clone();
    let progress = app.startup_gate.progressed;
    let inspected = app.startup_gate.inspected;
    drop(app.startup_state_inspected(old));
    assert_eq!(app.startup_gate.inspecting, active);
    assert_eq!(app.startup_gate.progressed, progress);
    assert_eq!(app.startup_gate.inspected, inspected);
    assert!(app.startup_gate.daemon.is_none());
    assert_eq!(reported(&app), None);
    drop(app.startup_state_inspected(current));
    assert!(app.startup_gate.inspecting.is_none());
    assert!(
        matches!(&app.startup_gate.daemon, Some(observed) if observed.daemon.state == StartupState::Ready)
    );
}

fn connected_chat(app: &mut Cagents) -> CardId {
    connected_chat_with_history(app, false)
}

fn connected_chat_with_history(app: &mut Cagents, history_loading: bool) -> CardId {
    let mut card = crate::app::tests::terminal_card_state();
    let CardKind::Terminal(terminal) = &card.kind else {
        unreachable!()
    };
    let session = crate::chat::Session {
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: terminal.launch.clone(),
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
        },
        proof: Some(fixture_proof()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let id = card.id;
    card.kind = CardKind::Chat(session.clone());
    Arc::make_mut(&mut app.durable).cards.push(card);
    app.cards.insert(id, CardRuntime::Loading);
    // Use the actual Ready adoption boundary without polling a connection or
    // driving Submit. The existing component handle owns only fixture peers.
    drop(app.start_chat(id, session, crate::chat::Intent::Restore, None, false));
    let ready = crate::app::chat_ui::ready_message_for_pending_switch(
        app,
        id,
        crate::chat::Snapshot {
            ready: true,
            history_loading,
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
            ..Default::default()
        },
    );
    drop(app.update(ready));
    assert!(!app.chat_start_pending(id));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&id) else {
        panic!("Ready did not install the current runtime");
    };
    assert_eq!(runtime.startup_host_proof(), Some(fixture_proof()));
    assert_eq!(runtime.restored_pane_ready(), Ok(!history_loading));
    id
}

fn old_proof() -> crate::model::HostSessionProof {
    let mut proof = fixture_proof();
    proof.host_incarnation = "previous-incarnation".into();
    proof
}

#[test]
fn current_connected_owner_refuses_old_or_incomplete_inspection_proofs() {
    for state in [
        StartupState::Ready,
        StartupState::Restoring { awaiting: 2 },
        StartupState::Retained {
            detail: DETAIL.into(),
        },
    ] {
        for component in 0..5 {
            let (mut app, _) = waiting(3);
            let mut answer = inspection(&mut app, Ok(state.clone()));
            let daemon = answer.outcome.as_mut().unwrap();
            match component {
                0 => daemon.proof.host_session_id = "previous-host".into(),
                1 => daemon.proof.boot_id = "previous-boot".into(),
                2 => daemon.proof.host_incarnation = "previous-incarnation".into(),
                3 => daemon.proof.host_incarnation = "".into(),
                4 => daemon.socket = PathBuf::from("/fixture/other-startup.sock"),
                _ => unreachable!(),
            }
            let card = connected_chat(&mut app);
            // The saved descriptor can lag the actual connected owner. A
            // descriptor-only comparison would admit these old answers.
            let stored = answer.outcome.as_ref().unwrap().proof.clone();
            set_saved_chat_proof(&mut app, card, stored);
            answer.context.owners = app.startup_connected_owners();
            app.startup_gate.inspecting = Some(answer.context.clone());
            let progress = app.startup_gate.progressed;
            drop(app.startup_state_inspected(answer));
            assert_eq!(reported(&app), None);
            assert!(app.startup_gate.daemon.is_none());
            assert_eq!(app.startup_gate.progressed, progress);
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("current runtime lost");
            };
            assert_eq!(runtime.startup_host_proof(), Some(fixture_proof()));
        }
    }
    let (mut app, _) = waiting(3);
    let card = connected_chat(&mut app);
    set_saved_chat_proof(&mut app, card, old_proof());
    drop(deliver(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    ));
    assert_eq!(
        reported_details(&app),
        Some(DETAIL),
        "a current matching failure must remain visible"
    );
}

fn set_saved_chat_proof(app: &mut Cagents, id: CardId, proof: crate::model::HostSessionProof) {
    let card = Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|card| card.id == id)
        .unwrap();
    let CardKind::Chat(session) = &mut card.kind else {
        panic!("fixture chat descriptor changed kind");
    };
    session.proof = Some(proof);
}

#[test]
fn losing_a_connected_owner_invalidates_cached_and_delayed_ready() {
    let (mut app, _) = waiting(10);
    let chat = connected_chat_with_history(&mut app, true);
    drop(app.verify_startup_gate());
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    assert!(app.startup_gate.daemon.is_some());
    let pending = app.startup_pending().unwrap();
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    let delayed = inspection(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    );
    assert_eq!(delayed.context.owners, vec![(chat, fixture_proof())]);
    // Model the existing projection's loss of a confirmed handle while the
    // same saved card remains pending. This is no provider/reattach execution.
    app.cards.insert(chat, CardRuntime::Loading);
    assert_eq!(app.startup_pending().unwrap(), pending);
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);
    assert!(app.startup_gate.daemon.is_none());
    let progress = app.startup_gate.progressed;
    drop(app.startup_state_inspected(delayed));
    assert_eq!(reported(&app), None);
    assert!(app.startup_gate.daemon.is_none());
    assert_eq!(app.startup_gate.progressed, progress);
    // A newly issued all-unknown observation is still useful. Only the old
    // nonempty owner basis is invalid; the absolute inventory cap is intact.
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    assert!(app.startup_gate.daemon.is_some());
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert!(reported(&app).unwrap().contains("made no progress"));
}

#[test]
fn cached_ready_is_revalidated_after_a_new_owner_connects() {
    let (mut app, _) = waiting(10);
    let mut old = inspection(&mut app, Ok(StartupState::Ready));
    old.outcome.as_mut().unwrap().proof = old_proof();
    drop(app.startup_state_inspected(old));
    assert!(app.startup_gate.daemon.is_some());
    connected_chat(&mut app);
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert_eq!(
        reported(&app),
        None,
        "old Ready cannot time a new owner's panes"
    );
    assert!(app.startup_gate.daemon.is_none());
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert!(
        reported(&app).unwrap().contains("made no progress"),
        "fresh matching Ready still detects a stall"
    );
}

fn end_terminals(app: &mut Cagents, cards: &[CardId]) {
    for card in &mut Arc::make_mut(&mut app.durable).cards {
        if cards.contains(&card.id) {
            let CardKind::Terminal(session) = &mut card.kind else {
                panic!("fixture terminal changed kind");
            };
            session.continuity = TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            };
        }
    }
}

#[test]
fn delayed_retained_cannot_override_current_readiness_or_save_completion() {
    for before_save in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, cards) = waiting(3);
        let (session, _) = SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("startup-save-fixture"),
        )
        .unwrap();
        let store = session.operation_store();
        app.session = Some(session);
        app.settings_path = Some(directory.path().join("settings.toml"));
        app.settings_write_blocked = false;
        let mut old = inspection(
            &mut app,
            Ok(StartupState::Retained {
                detail: DETAIL.into(),
            }),
        );
        old.outcome.as_mut().unwrap().proof = old_proof();
        let duplicate = old.clone();
        let chat = connected_chat(&mut app);
        let expected_cards: Vec<_> = app.durable.cards.iter().map(|card| card.id).collect();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&chat) else {
            unreachable!()
        };
        let expected_session = runtime.session.clone();
        end_terminals(&mut app, &cards);
        assert_eq!(app.startup_pane_check(), Ok(true));
        let save = if before_save {
            assert_eq!(app.startup_state_inspected(old).units(), 0);
            assert!(!app.startup_gate.saving && !app.startup_gate.reported);
            save_on_ready(&mut app)
        } else {
            let save = save_on_ready(&mut app);
            drop(app.startup_state_inspected(old));
            save
        };
        assert!(app.startup_gate.saving && !app.startup_gate.reported);
        assert!(app.startup_gate.inspecting.is_none() && app.startup_gate.daemon.is_none());
        drop(app.startup_state_inspected(duplicate.clone()));
        assert!(
            app.startup_gate.daemon.is_none(),
            "a waiting-phase answer cannot reenter a save"
        );
        let completed = saved_message(save);
        let Message::StartupPanesSaved(settings, workspace) = completed else {
            unreachable!()
        };
        assert!(workspace.is_ok(), "{workspace:?}");
        let settings = settings.unwrap();
        let generation = settings.generation;
        let path = settings.path.clone();
        assert!(path.starts_with(directory.path()));
        assert!(path.is_file());
        assert!(store.load_window_view().unwrap().is_some());
        drop(app.startup_panes_saved(Ok(settings), workspace));
        assert!(app.startup_gate.reported && !app.startup_gate.saving);
        assert_eq!(app.settings_ack_generation, generation);
        assert_eq!(app.settings_path, Some(path));
        assert!(app.status.as_ref().is_none_or(|status| {
            !status.message.contains(DETAIL) && status.details.as_deref() != Some(DETAIL)
        }));
        drop(app.startup_state_inspected(duplicate));
        assert!(app.startup_gate.daemon.is_none());
        assert_eq!(app.settings_ack_generation, generation);
        assert_eq!(app.startup_pane_check(), Ok(true));
        assert_eq!(
            app.durable
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            expected_cards
        );
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&chat) else {
            panic!("save or delayed inspection lost the connected chat");
        };
        assert_eq!(runtime.session, expected_session);
        assert_eq!(runtime.startup_host_proof(), Some(fixture_proof()));
        assert_eq!(runtime.restored_pane_ready(), Ok(true));
        for card in &cards {
            assert!(
                matches!(app.cards.get(card), Some(CardRuntime::Loading)),
                "gate must not replace a pane runtime"
            );
        }
    }
}

/// Answers the inspection the gate itself reserved, as its worker would.
fn answer_reserved(app: &mut Cagents, state: Result<StartupState, String>) -> Task<Message> {
    let context = app
        .startup_gate
        .inspecting
        .clone()
        .expect("the gate reserved no inspection");
    assert_eq!(app.startup_gate.outstanding, Some(context.ticket));
    let socket = app.startup_gate.fixture_endpoint.clone().unwrap();
    app.startup_state_inspected(Inspection {
        context,
        outcome: state.map(|state| Observation {
            socket,
            proof: fixture_proof(),
            state,
        }),
    })
}

/// Settles the gate on a fresh Ready answer for exactly its current owners and
/// returns the save it starts. A fixture's earlier worker that still holds the
/// slot is answered past it, through a context built for the settled phase.
fn save_on_ready(app: &mut Cagents) -> Task<Message> {
    drop(app.verify_startup_gate());
    assert!(
        !app.startup_gate.saving,
        "settled panes alone must not save"
    );
    if app.startup_gate.inspecting.is_some() {
        drop(answer_reserved(app, Ok(StartupState::Ready)));
    } else {
        drop(deliver(app, Ok(StartupState::Ready)));
    }
    let save = app.verify_startup_gate();
    assert!(app.startup_gate.saving);
    save
}

/// Poll only the gate's original save Task, never its poll/inspection/provider
/// Tasks. The session and settings writers both use the owned private root.
fn saved_message(task: Task<Message>) -> Message {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(saved_output(task))
}

async fn saved_output(task: Task<Message>) -> Message {
    use cosmic::iced::futures::StreamExt;
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut stream = cosmic::iced::runtime::task::into_stream(task).expect("save task");
        while let Some(action) = stream.next().await {
            if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                message @ Message::StartupPanesSaved(..),
            )) = action
            {
                return message;
            }
        }
        panic!("the original save Task emitted no completion");
    })
    .await
    .expect("private workspace save did not complete")
}

#[test]
fn a_retired_worker_keeps_occupancy_until_its_original_completion() {
    use cosmic::iced::futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Declare the owned root before the app so its session writer joins first.
    let directory = tempfile::tempdir().unwrap();
    let (mut app, cards) = waiting(3);
    let (session, _) = SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("startup-worker-fixture"),
    )
    .unwrap();
    app.session = Some(session);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let original_terminal = app.durable.cards[0].clone();
    let context = app.reserve_startup_inspection().expect("first reservation");
    let ticket = context.ticket;
    let socket = app.startup_gate.fixture_endpoint.clone().unwrap();
    let produced = Arc::new(AtomicUsize::new(0));
    let entered = Arc::clone(&produced);
    let (release, held) = std::sync::mpsc::channel();
    let original = inspection_task(context, false, move || {
        entered.fetch_add(1, Ordering::SeqCst);
        // Dropping the sender on any parent panic also unblocks this worker.
        held.recv_timeout(Duration::from_secs(40))
            .map_err(|error| error.to_string())?;
        Ok(Observation {
            socket,
            proof: fixture_proof(),
            state: StartupState::Ready,
        })
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let mut stream = cosmic::iced::runtime::task::into_stream(original).unwrap();
            // Drive the actual production future/spawn_blocking boundary, with
            // a private closure instead of an RPC, until it enters the hold.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    tokio::select! {
                        output = stream.next() => panic!("held probe returned early: {}", output.is_some()),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {
                            if produced.load(Ordering::SeqCst) == 1 {
                                break;
                            }
                        }
                    }
                }
            })
            .await
            .expect("original inspection worker did not enter");
            end_terminals(&mut app, &cards);
            // The held worker keeps the settled phase from asking, so the
            // gate saves once an earlier worker and its own would be done.
            drop(app.verify_startup_gate());
            assert!(!app.startup_gate.saving);
            elapse(&mut app, SETTLED_ANSWER_BOUND * 2);
            let save = app.verify_startup_gate();
            assert!(app.startup_gate.saving);
            assert!(app.startup_gate.inspecting.is_none());
            assert_eq!(app.startup_gate.outstanding, Some(ticket));
            // The existing save consumer explicitly permits becoming pending
            // again while its original flush is in flight.
            Arc::make_mut(&mut app.durable).cards[0] = original_terminal;
            let Message::StartupPanesSaved(settings, workspace) = saved_output(save).await else {
                unreachable!()
            };
            assert!(settings.is_ok() && workspace.is_ok());
            drop(app.startup_panes_saved(settings, workspace));
            assert!(!app.startup_gate.saving && !app.startup_gate.reported);
            assert_eq!(app.startup_gate.outstanding, Some(ticket));
            // Eliminate the throttle as an alternative reason for refusal.
            app.startup_gate.inspected = app.startup_gate.inspected.map(|at| {
                at.checked_sub(INSPECT_EVERY + Duration::from_secs(1)).unwrap()
            });
            assert!(app.startup_gate.inspected.unwrap().elapsed() >= INSPECT_EVERY);
            assert!(app.reserve_startup_inspection().is_none());
            assert_eq!(produced.load(Ordering::SeqCst), 1);
            let progress = app.startup_gate.progressed;
            release.send(()).unwrap();
            let retired = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        Message::StartupStateInspected(inspection),
                    )) = action
                    {
                        return inspection;
                    }
                }
                panic!("original worker emitted no inspection completion");
            })
            .await
            .expect("released original worker did not complete");
            drop(app.startup_state_inspected(retired));
            assert!(app.startup_gate.outstanding.is_none());
            assert!(app.startup_gate.daemon.is_none());
            assert_eq!(app.startup_gate.progressed, progress);
            assert_eq!(reported(&app), None);
            let next = app.reserve_startup_inspection().expect("next reservation");
            assert!(next.ticket > ticket);
            let entered = Arc::clone(&produced);
            let socket = app.startup_gate.fixture_endpoint.clone().unwrap();
            let next = inspection_task(next, false, move || {
                entered.fetch_add(1, Ordering::SeqCst);
                Ok(Observation {
                    socket,
                    proof: fixture_proof(),
                    state: StartupState::Ready,
                })
            });
            let mut stream = cosmic::iced::runtime::task::into_stream(next).unwrap();
            let completion = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        Message::StartupStateInspected(inspection),
                    )) = action
                    {
                        return inspection;
                    }
                }
                panic!("next worker emitted no completion");
            })
            .await
            .expect("next inspection worker did not complete");
            drop(app.startup_state_inspected(completion));
            assert_eq!(produced.load(Ordering::SeqCst), 2);
            assert!(app.startup_gate.outstanding.is_none());
            assert!(app.startup_gate.daemon.is_some());
        });
}

#[test]
fn a_stale_inspection_does_not_hide_a_real_save_error() {
    let (mut app, cards) = waiting(3);
    let old = inspection(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    );
    end_terminals(&mut app, &cards);
    assert!(app.session.is_none());
    drop(app.startup_state_inspected(old));
    let save = save_on_ready(&mut app);
    let Message::StartupPanesSaved(settings, workspace) = saved_message(save) else {
        unreachable!()
    };
    assert_eq!(
        settings.as_ref().unwrap_err(),
        "workspace storage unavailable"
    );
    assert_eq!(
        workspace.as_ref().unwrap_err(),
        "workspace storage unavailable"
    );
    drop(app.startup_panes_saved(settings, workspace));
    let reason = reported(&app).unwrap();
    assert_readable(reason);
    assert!(reason.starts_with("Saved panes reconnected"), "{reason}");
    assert!(reason.contains("workspace storage unavailable"));
    assert!(!reason.contains(DETAIL));
}

#[test]
fn an_inspection_that_sees_panes_move_leaves_polling_to_the_running_check() {
    let (mut app, cards) = waiting(3);
    let stale = inspection(&mut app, Ok(StartupState::Ready));
    app.cards.insert(
        cards[0],
        CardRuntime::Failed("Fixture attach failed".into()),
    );
    // The gate's own check is already scheduled. Each inspection that saw a
    // pane settle must not start another 200 ms poll on the UI thread.
    assert_eq!(app.startup_state_inspected(stale).units(), 0);
    assert_eq!(reported(&app), None);
    assert!(app.startup_gate.outstanding.is_none());
    // The running check still records that move as progress.
    elapse(&mut app, Duration::from_secs(1));
    let progressed = app.startup_gate.progressed;
    app.startup_gate.inspected = Some(Instant::now());
    drop(app.verify_startup_gate());
    assert_ne!(app.startup_gate.progressed, progressed);
}

/// Settles every pane and checks the gate asked the daemon through its own
/// reservation, bound to the settled phase.
fn settle(app: &mut Cagents, cards: &[CardId]) {
    end_terminals(app, cards);
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving && app.startup_gate.daemon.is_none());
    assert_eq!(reported(app), None);
    let reserved = app
        .startup_gate
        .inspecting
        .as_ref()
        .expect("settled gate did not ask");
    assert_eq!(reserved.pending, None);
    assert_eq!(reserved.owners, app.startup_connected_owners());
    assert!(app.startup_gate.asked.is_some());
}

#[test]
fn settled_panes_wait_for_the_daemon_and_report_a_retained_restore() {
    // A terminal can attach while the daemon keeps its original owners, so
    // panes that settled do not prove the update recovered them.
    let (mut app, cards) = waiting(2);
    connected_chat(&mut app);
    // Ready from the waiting phase does not vouch for the settled one.
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    assert!(app.startup_gate.daemon.is_some());
    app.startup_gate.inspected = None;
    settle(&mut app, &cards);
    drop(answer_reserved(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    ));
    let reason = reported(&app).expect("retained recovery must answer the launcher");
    assert_readable(reason);
    assert_eq!(reported_details(&app), Some(DETAIL));
    assert!(!app.startup_gate.saving);
}

#[test]
fn a_slow_settled_answer_still_reports_a_retained_restore() {
    let (mut app, cards) = waiting(2);
    // A waiting-phase worker holds the slot when the panes settle.
    let earlier = inspection(&mut app, Ok(StartupState::Ready));
    end_terminals(&mut app, &cards);
    drop(app.verify_startup_gate());
    assert!(app.startup_gate.asked.is_none(), "the slot was held");
    elapse(&mut app, Duration::from_secs(4));
    drop(app.startup_state_inspected(earlier));
    assert!(app.startup_gate.daemon.is_none());
    app.startup_gate.inspected = None;
    drop(app.verify_startup_gate());
    assert!(app.startup_gate.asked.is_some());
    // The settled worker takes almost its whole bound to answer.
    elapse(&mut app, SETTLED_ANSWER_BOUND - Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving);
    drop(answer_reserved(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    ));
    assert_eq!(reported_details(&app), Some(DETAIL));
}

#[test]
fn settled_panes_save_once_the_daemon_is_ready_unreachable_or_past_its_bound() {
    for answer in [
        Some(Ok(StartupState::Ready)),
        Some(Err("daemon socket connection deadline elapsed".to_owned())),
        None,
    ] {
        let (mut app, cards) = waiting(2);
        settle(&mut app, &cards);
        match answer {
            Some(answer) => drop(answer_reserved(&mut app, answer)),
            None => {
                elapse(&mut app, SETTLED_ANSWER_BOUND - Duration::from_secs(1));
                drop(app.verify_startup_gate());
                assert!(!app.startup_gate.saving);
                elapse(&mut app, Duration::from_secs(1));
            }
        }
        drop(app.verify_startup_gate());
        assert!(app.startup_gate.saving);
        assert_eq!(reported(&app), None);
    }
}

#[test]
fn settled_panes_wait_out_a_restore_until_the_inventory_cap() {
    let (mut app, cards) = waiting(2);
    settle(&mut app, &cards);
    drop(answer_reserved(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 1 }),
    ));
    // A restore still under way can end Retained; it is not a verdict,
    // even past the bound a silent daemon gets and within its budget.
    elapse(&mut app, SETTLED_ANSWER_BOUND + Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving);
    assert_eq!(reported(&app), None);
    // The budget fixed when the window opened does not shrink as panes settle.
    elapse(&mut app, launcher_budget(2));
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("the cap bounds a restore");
    assert_readable(reason);
    assert!(
        reason.contains("still restoring 1 saved session"),
        "{reason}"
    );
    assert!(!app.startup_gate.saving);

    // Its finishing lets the gate save.
    let (mut app, cards) = waiting(2);
    settle(&mut app, &cards);
    drop(answer_reserved(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 1 }),
    ));
    drop(app.verify_startup_gate());
    app.startup_gate.inspected = None;
    drop(app.verify_startup_gate());
    drop(answer_reserved(&mut app, Ok(StartupState::Ready)));
    drop(app.verify_startup_gate());
    assert!(app.startup_gate.saving);
}

#[test]
fn an_answer_to_an_earlier_pending_set_stays_rejected_after_it_returns() {
    let (mut app, cards) = waiting(3);
    let original = app.startup_pending().unwrap();
    let earlier = inspection(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    );
    // Hold the throttle so no new reservation replaces the earlier one.
    app.startup_gate.inspected = Some(Instant::now());
    let failed = app.cards.insert(
        cards[0],
        CardRuntime::Failed("Fixture attach failed".into()),
    );
    drop(app.verify_startup_gate());
    app.cards.insert(cards[0], failed.unwrap());
    assert_eq!(app.startup_pending().unwrap(), original);
    drop(app.verify_startup_gate());
    drop(app.startup_state_inspected(earlier));
    assert_eq!(reported(&app), None);
    assert!(app.startup_gate.daemon.is_none());
}

#[test]
fn notices_name_failed_panes_by_title_and_say_what_to_do() {
    let (mut app, cards) = waiting(1);
    app.cards.insert(
        cards[0],
        CardRuntime::Failed("Fixture attach failed".into()),
    );
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("a failed pane answers the launcher");
    assert_readable(reason);
    assert!(
        reason.starts_with("1 saved pane could not reconnect: \u{201c}Terminal\u{201d}. Open it to see why and retry"),
        "{reason}"
    );
    assert!(!reason.contains(&cards[0].to_string()), "{reason}");
    let long = pane_title(&"Very long\ntitle ".repeat(40));
    assert!(!long.contains('\n'));
    assert_eq!(long.chars().count(), MAX_PANE_TITLE_CHARS + 1);
}

#[test]
fn an_inspection_that_sees_panes_settle_leaves_the_save_to_the_running_check() {
    let (mut app, cards) = waiting(3);
    let stale = inspection(&mut app, Ok(StartupState::Ready));
    end_terminals(&mut app, &cards);
    // The gate's own check is scheduled beside every inspection; starting a
    // check here could leave two poll loops once a save finds panes pending.
    assert_eq!(app.startup_state_inspected(stale).units(), 0);
    assert!(!app.startup_gate.saving);
    assert_eq!(reported(&app), None);
    assert!(app.startup_gate.outstanding.is_none());
}

#[test]
fn a_waiting_phase_failure_does_not_excuse_the_settled_answer() {
    let (mut app, cards) = waiting(2);
    drop(deliver(
        &mut app,
        Err("daemon socket connection deadline elapsed".into()),
    ));
    assert!(app.startup_gate.unavailable);
    app.startup_gate.inspected = None;
    settle(&mut app, &cards);
    assert!(!app.startup_gate.unavailable);
    drop(answer_reserved(
        &mut app,
        Ok(StartupState::Retained {
            detail: DETAIL.into(),
        }),
    ));
    assert_eq!(reported_details(&app), Some(DETAIL));
}

#[test]
fn a_settled_answer_dropped_with_its_owner_is_not_silence() {
    let (mut app, cards) = waiting(2);
    let chat = connected_chat(&mut app);
    app.startup_gate.inspected = None;
    settle(&mut app, &cards);
    drop(answer_reserved(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 1 }),
    ));
    // Past the bound a silent daemon gets, within this window's budget.
    elapse(&mut app, SETTLED_ANSWER_BOUND + Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving);
    // Closing the connected chat changes the owners the answer was for.
    for card in &mut Arc::make_mut(&mut app.durable).cards {
        if card.id == chat {
            card.existence = crate::model::Existence::Closing {
                operation_id: crate::model::OperationId::new(),
            };
        }
    }
    app.startup_gate.inspected = Some(Instant::now());
    drop(app.verify_startup_gate());
    assert!(app.startup_gate.daemon.is_none(), "the owners changed");
    assert!(!app.startup_gate.saving, "a dropped Restoring is not Ready");
    assert_eq!(reported(&app), None);
    // The cap still bounds the wait for a current answer.
    elapse(&mut app, launcher_budget(2));
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("the cap bounds the wait");
    assert_readable(reason);
    assert!(reason.contains("did not confirm"), "{reason}");
}

#[test]
fn a_reachable_daemon_for_other_owners_is_not_silence() {
    let (mut app, cards) = waiting(2);
    connected_chat(&mut app);
    app.startup_gate.inspected = None;
    settle(&mut app, &cards);
    let context = app.startup_gate.inspecting.clone().unwrap();
    drop(app.startup_state_inspected(Inspection {
        context,
        outcome: Ok(Observation {
            socket: app.startup_gate.fixture_endpoint.clone().unwrap(),
            proof: old_proof(),
            state: StartupState::Ready,
        }),
    }));
    assert!(app.startup_gate.mismatched && app.startup_gate.daemon.is_none());
    elapse(&mut app, SETTLED_ANSWER_BOUND * 2 + Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving);
    assert_eq!(reported(&app), None);
    // The cap still bounds the wait for an answer about these owners.
    elapse(&mut app, Duration::from_secs(120));
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving);
    let reason = reported(&app).expect("the cap bounds the wait");
    assert_readable(reason);
    assert!(reason.contains("did not confirm"), "{reason}");
}

#[test]
fn pane_notices_fit_one_launcher_handshake_line_at_their_longest() {
    let (mut app, cards) = waiting(NAMED_PANES + 3);
    for card in &mut Arc::make_mut(&mut app.durable).cards {
        card.title = "\u{1d11e}".repeat(MAX_PANE_TITLE_CHARS * 2);
    }
    let pending = Pending {
        admission: false,
        panes: cards.clone(),
    };
    let stalled = app.stalled_reason(&pending);
    let reconnecting = app.reconnecting_reason(&pending);
    for card in &cards {
        app.cards
            .insert(*card, CardRuntime::Failed("Fixture attach failed".into()));
    }
    let failed = app.startup_pending().unwrap_err();
    for reason in [stalled, reconnecting, failed] {
        let line = format!("{}{reason}", instance::UI_LAUNCH_ERROR_PREFIX);
        assert!(line.len() < 4096, "{}", line.len());
        assert!(!line.contains('\n'));
        assert!(line.contains("and 3 more"), "{line}");
    }
}

#[test]
fn a_failed_inspection_after_an_answer_is_not_silence() {
    let (mut app, cards) = waiting(2);
    connected_chat(&mut app);
    app.startup_gate.inspected = None;
    settle(&mut app, &cards);
    drop(answer_reserved(
        &mut app,
        Ok(StartupState::Restoring { awaiting: 1 }),
    ));
    app.startup_gate.inspected = None;
    drop(app.verify_startup_gate());
    // A daemon busy restoring can miss one control IO bound.
    drop(answer_reserved(
        &mut app,
        Err("fixture control timeout".into()),
    ));
    assert!(app.startup_gate.unavailable && app.startup_gate.daemon.is_none());
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving, "its last word was Restoring");
    assert_eq!(reported(&app), None);
}

#[test]
fn a_reachable_daemon_for_other_owners_supersedes_an_unreachable_one() {
    let (mut app, cards) = waiting(2);
    connected_chat(&mut app);
    app.startup_gate.inspected = None;
    settle(&mut app, &cards);
    drop(answer_reserved(
        &mut app,
        Err("fixture daemon starting".into()),
    ));
    assert!(app.startup_gate.unavailable);
    // The next answer arrives before the gate's own check runs.
    app.startup_gate.inspected = None;
    let context = app.reserve_startup_inspection().unwrap();
    drop(app.startup_state_inspected(Inspection {
        context,
        outcome: Ok(Observation {
            socket: app.startup_gate.fixture_endpoint.clone().unwrap(),
            proof: old_proof(),
            state: StartupState::Ready,
        }),
    }));
    assert!(!app.startup_gate.unavailable, "the daemon was reachable");
    drop(app.verify_startup_gate());
    assert!(!app.startup_gate.saving);
    assert_eq!(reported(&app), None);
}

#[test]
fn a_single_waiting_pane_is_named_in_the_singular() {
    let (app, cards) = waiting(1);
    let pending = Pending {
        admission: false,
        panes: cards,
    };
    for reason in [
        app.stalled_reason(&pending),
        app.reconnecting_reason(&pending),
    ] {
        assert!(reason.contains("1 saved pane "), "{reason}");
        assert!(
            reason.contains(
                ". It stays in this window, which remains available; open it to see its status."
            ),
            "{reason}"
        );
    }
}

#[test]
fn a_saved_chat_that_never_gets_a_runtime_is_named_once_the_daemon_is_ready() {
    // A provider the daemon relaunched can leave its saved chat without a
    // runtime; the gate names it rather than waiting out the inventory cap.
    let (mut app, cards) = waiting(2);
    connected_chat(&mut app);
    let mut card = crate::app::tests::terminal_card_state();
    let CardKind::Terminal(terminal) = &card.kind else {
        unreachable!()
    };
    let session = crate::chat::Session {
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: terminal.launch.clone(),
            session_id: Some("22345678-1234-4234-8234-123456789abc".into()),
        },
        proof: Some(fixture_proof()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let stuck = card.id;
    card.kind = CardKind::Chat(session);
    card.title = "Relaunched Codex".into();
    Arc::make_mut(&mut app.durable).cards.push(card);
    end_terminals(&mut app, &cards);
    drop(app.verify_startup_gate());
    assert_eq!(
        app.startup_gate
            .pending
            .as_ref()
            .map(|pending| pending.panes.clone()),
        Some(vec![stuck])
    );
    drop(deliver(&mut app, Ok(StartupState::Ready)));
    assert!(app.startup_gate.daemon.is_some(), "Ready for these owners");
    // Ninety seconds without progress, inside a larger window's inventory
    // budget (two counted chats here give only 90 s).
    let progressed = app.startup_gate.progressed.unwrap();
    app.startup_gate.progressed = Some(
        progressed
            .checked_sub(NO_PROGRESS + Duration::from_secs(1))
            .unwrap(),
    );
    assert!(!app.startup_budget_spent(Instant::now()));
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("a stalled chat answers the launcher");
    assert_readable(reason);
    assert!(
        reason.starts_with(
            "1 saved pane made no progress reconnecting for 90 seconds: \u{201c}Relaunched Codex\u{201d}. It stays"
        ),
        "{reason}"
    );
}

/// Panes are refused while the daemon restores, so only the time after it
/// finishes counts toward reporting one, and a slow restore is not a failure.
#[test]
fn a_pane_refused_while_the_daemon_restores_is_not_counted_against_it() {
    let (mut app, cards) = waiting(1);
    let terminal = match &app.card_state(cards[0]).unwrap().kind {
        CardKind::Terminal(session) => session.id,
        _ => unreachable!(),
    };
    app.startup_attach_failed(cards[0], terminal, "restoring".into());
    let answer = inspection(&mut app, Ok(StartupState::Restoring { awaiting: 1 }));
    app.startup_gate.retries.expire_window();
    drop(app.startup_state_inspected(answer));
    assert!(!app.startup_gate.reported, "{:?}", reported(&app));
    assert_eq!(app.startup_pane_check(), Ok(false));

    app.startup_attach_failed(cards[0], terminal, "refused for good".into());
    let answer = inspection(&mut app, Ok(StartupState::Ready));
    drop(app.startup_state_inspected(answer));
    app.startup_gate.retries.expire_window();
    assert!(
        app.startup_pane_check().is_err(),
        "time after the restore counts"
    );
}

fn card_with_title(app: &mut Cagents, card: CardId, title: &str) {
    app.card_state_mut(card).unwrap().title = title.to_owned();
}

/// Opening a failed pane offers a retry, and opening a held one only says what
/// it waits for, so a report that names both says so of each.
#[test]
fn a_report_tells_held_panes_from_failed_ones() {
    let (mut app, cards) = waiting(2);
    card_with_title(&mut app, cards[0], "Failed");
    card_with_title(&mut app, cards[1], "Held");
    app.cards.insert(cards[0], CardRuntime::Completed);
    app.shared_views.attached.insert(cards[1]);
    app.workspace_model.identity = crate::workspace_model::NO_REGISTRY.to_owned();
    assert!(app.startup_pane_held(cards[1]));
    assert!(!app.startup_pane_held(cards[0]));

    let report = app.startup_incomplete(&cards, &[], None);
    assert!(
        report.contains(
            "1 saved pane could not reconnect: \u{201c}Failed\u{201d}. Open it to see why and retry."
        ),
        "{report}"
    );
    assert!(
        report.contains(
            "1 saved pane is waiting for the shared workspace: \u{201c}Held\u{201d}. Nothing is needed from you now; once the workspace is ready, it reconnects when you open it."
        ),
        "{report}"
    );

    let held = app.startup_incomplete(&cards[1..], &[], None);
    assert!(!held.contains("retry"), "{held}");
}

/// A report that runs out of time still says what a held pane waits for, and
/// keeps the pane with an attempt under way apart from it.
#[test]
fn a_budget_report_gives_held_panes_their_next_step() {
    let (mut app, cards) = waiting(2);
    card_with_title(&mut app, cards[0], "Attaching");
    card_with_title(&mut app, cards[1], "Held");
    app.workspace_model.identity = crate::workspace_model::NO_REGISTRY.to_owned();
    app.shared_views.attached.extend(cards.iter().copied());
    app.begin_terminal_start(cards[0]);
    assert!(!app.startup_pane_held(cards[0]));
    assert!(app.startup_pane_held(cards[1]));

    let pending = app.startup_pending().unwrap().unwrap();
    let report = app.reconnecting_reason(&pending);
    assert!(
        report.contains("1 saved pane did not reconnect in time: \u{201c}Attaching\u{201d}."),
        "{report}"
    );
    assert!(
        report.contains(
            "1 saved pane is waiting for the shared workspace: \u{201c}Held\u{201d}. Nothing is needed from you now; once the workspace is ready, it reconnects when you open it."
        ),
        "{report}"
    );
    assert!(
        report.contains("The update has not been verified."),
        "{report}"
    );
}

/// A shared chat the registry has not admitted opens to its saved draft, with
/// no retry to offer.
#[test]
fn a_chat_awaiting_admission_is_held() {
    let (mut app, _) = waiting(1);
    let chat = connected_chat(&mut app);
    let CardKind::Chat(session) = app.card_state(chat).unwrap().kind.clone() else {
        unreachable!()
    };
    assert!(!app.startup_pane_held(chat));
    app.await_shared_chat_admission(chat, session);
    assert!(app.chat_awaiting_admission(chat));
    assert!(app.startup_pane_held(chat));
    let report = app.startup_incomplete(&[chat], &[], None);
    assert!(
        report.contains(
            "It reopens on its own once the workspace is ready; until then, opening it shows any unsent text."
        ),
        "{report}"
    );
    assert!(!report.contains("retry"), "{report}");
}

/// Past its limit the launcher closes the window, so a report at its longest
/// is cut rather than sent whole.
#[test]
fn the_longest_report_fits_what_the_launcher_accepts() {
    let (mut app, cards) = waiting(16);
    for card in &cards {
        card_with_title(&mut app, *card, &"\u{1F980}".repeat(100));
    }
    let detail = "\u{1F980}".repeat(1000);
    let report = app.startup_incomplete(&cards[..8], &cards[8..], Some(&detail));
    assert!(report.len() > 4096, "not its longest: {}", report.len());
    let bounded = bounded_report(report);
    assert!(bounded.len() <= MAX_REPORT_BYTES, "{}", bounded.len());
    assert!(bounded.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX));
}

/// A pane that has already failed stays named when another is still waiting
/// as the budget runs out.
#[test]
fn a_budget_report_also_names_panes_that_failed() {
    let (mut app, cards) = waiting(2);
    card_with_title(&mut app, cards[0], "Failed");
    card_with_title(&mut app, cards[1], "Waiting");
    app.cards.insert(cards[0], CardRuntime::Completed);
    let pending = app
        .startup_pending()
        .unwrap()
        .expect("one is still waiting");
    let report = app.reconnecting_reason(&pending);
    assert!(report.contains("\u{201c}Waiting\u{201d}"), "{report}");
    assert!(report.contains("\u{201c}Failed\u{201d}"), "{report}");
    assert!(report.contains("Open it to see why and retry"), "{report}");
}

/// A daemon restoring for most of the launcher's own allowance is not failed
/// by a window with no panes of its own.
#[test]
fn an_empty_window_waits_out_most_of_the_launchers_allowance() {
    assert_eq!(launcher_budget(0), Duration::from_secs(25));
}

/// The registry store being unavailable is not something to wait out: a chat
/// must not be told it reopens on its own, and the report must say why.
#[test]
fn a_chat_with_the_store_unavailable_is_not_told_it_reopens_on_its_own() {
    let (mut app, _) = waiting(1);
    let chat = connected_chat(&mut app);
    let CardKind::Chat(session) = app.card_state(chat).unwrap().kind.clone() else {
        unreachable!()
    };
    app.await_shared_chat_admission(chat, session);
    app.shared_views.registry_unavailable = true;
    assert!(!app.startup_pane_held(chat));
    let report = app.startup_incomplete(&[chat], &[], None);
    assert!(
        report.contains("cannot reopen while the shared workspace is unavailable"),
        "{report}"
    );
    assert!(
        report.contains(crate::workspace_model::REGISTRY_UNAVAILABLE),
        "{report}"
    );
    assert!(!report.contains("on its own"), "{report}");
    assert!(!report.contains("Open it"), "{report}");
}

/// What an updater from before relaunch grouping writes, in a stopped update.
const OLDER_RETAINED_JOB: &str = "primary: recovery-incomplete: the terminal service could not reconnect saved sessions after the update: Upgrade restore retained original ownership: terminal dimensions or scrollback are outside supported bounds; checkpoint retained at /home/someone/.local/state/daemon-upgrade.json. Their original processes were kept and nothing was closed or deleted; this window remains available. The update has not been verified.; window-2: recovery-incomplete: the terminal service could not reconnect saved sessions after the update: Upgrade restore retained original ownership: terminal dimensions or scrollback are outside supported bounds; checkpoint retained at /home/someone/.local/state/daemon-upgrade.json. Their original processes were kept and nothing was closed or deleted; this window remains available. The update has not been verified.";

#[test]
fn a_stopped_update_leads_with_its_steps_whichever_build_wrote_it() {
    let current = OLDER_RETAINED_JOB.split("; window-2: ").next().unwrap();
    let current = current
        .strip_prefix("primary: recovery-incomplete: the")
        .map(|rest| format!("The{rest}"))
        .unwrap();
    for message in [OLDER_RETAINED_JOB, current.as_str()] {
        let failure = update_failure(message);
        assert!(failure.retained);
        assert_eq!(failure.notice.message, RETAINED_NOTICE);
        assert!(failure.notice.message.contains("cagents daemon stop"));
        assert!(!failure.summary.contains('/'), "{}", failure.summary);
        let details = failure.notice.details.as_deref().unwrap();
        assert!(details.contains("checkpoint retained at"), "{details}");
        assert!(!details.contains("recovery-incomplete"), "{details}");
        assert!(!details.contains("window-2"), "{details}");
    }
}

#[test]
fn a_failed_update_reads_without_window_slots_or_the_handshake_prefix() {
    let report = "recovery-incomplete: 1 saved pane could not reconnect: \u{201c}A\u{201d}. Open it to see why and retry. Details: refused; try later.";
    let failure = update_failure(&format!("primary: {report}; window-2: {report}"));
    assert!(!failure.retained);
    assert_eq!(
        failure.notice.message,
        "1 saved pane could not reconnect: \u{201c}A\u{201d}. Open it to see why and retry."
    );
    assert_eq!(
        failure.notice.details.as_deref(),
        Some("refused; try later")
    );

    let different = update_failure(&format!("primary: {report}; window-2: no display"));
    assert!(
        different.notice.message.starts_with("primary: "),
        "{}",
        different.notice.message
    );
    assert!(
        different.notice.message.contains("window-2: No display")
            || different
                .notice
                .details
                .as_deref()
                .is_some_and(|d| d.contains("window-2: No display"))
    );

    let other = update_failure("installed daemon startup failed; see the log");
    assert_eq!(
        other.notice.message,
        "installed daemon startup failed; see the log"
    );
    assert_eq!(
        other.summary,
        "installed daemon startup failed; see the log"
    );
}
