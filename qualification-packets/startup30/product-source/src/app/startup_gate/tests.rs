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
    let pending = app.startup_pending().unwrap().unwrap();
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
            pending: app.startup_gate.pending.clone().unwrap_or(Pending {
                admission: false,
                panes: vec![],
            }),
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
    // Launchers keep the window open only for this error form.
    assert!(
        reason.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX),
        "{reason}"
    );
    assert!(reason.contains(DETAIL), "{reason}");
    assert!(reason.contains("this window remains available"), "{reason}");
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
    assert!(
        reason.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX),
        "{reason}"
    );
    assert!(reason.contains("made no progress"), "{reason}");
    assert!(reason.contains(&cards[0].to_string()), "{reason}");
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
    elapse(&mut app, Duration::from_secs(59));
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
    assert!(reason.contains("still reconnecting"), "{reason}");
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
    assert!(
        reported(&app).unwrap().contains(DETAIL),
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
fn open_quarantine_precedes_waivers_but_closed_evidence_stays_closed() {
    for chat in [false, true] {
        let (mut app, cards) = waiting(1);
        let id = cards[0];
        if chat {
            let card = app.card_state_mut(id).unwrap();
            let CardKind::Terminal(terminal) = &card.kind else {
                unreachable!()
            };
            let session = crate::chat::Session {
                id: terminal.id,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: terminal.launch.clone(),
                    session_id: None,
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            };
            assert!(session.is_unstarted());
            card.kind = CardKind::Chat(session);
        } else {
            end_terminals(&mut app, &[id]);
        }
        assert_eq!(app.startup_pane_check(), Ok(true));
        let original = app.card_state(id).unwrap().clone();
        app.shared_views.quarantined.insert(id);
        Arc::make_mut(&mut app.durable)
            .quarantined_panes
            .insert(id, original.kind.clone());
        let reason = app.startup_pending().unwrap_err();
        assert!(reason.starts_with("recovery-incomplete:"), "{reason}");
        assert!(reason.contains(&id.to_string()), "{reason}");
        assert_eq!(app.card_state(id), Some(&original));
        assert!(app.shared_views.quarantined.contains(&id));
        assert_eq!(app.durable.quarantined_panes.get(&id), Some(&original.kind));

        // Closed history retains its evidence without becoming an open failure.
        app.card_state_mut(id).unwrap().existence =
            super::super::project_ui::removed_existence("fixture closed");
        let closed = app.card_state(id).unwrap().clone();
        assert_eq!(app.startup_pending(), Ok(None));
        assert_eq!(app.startup_pane_check(), Ok(true));
        assert_eq!(app.card_state(id), Some(&closed));
        assert!(!closed.existence.is_open());
        assert!(app.shared_views.quarantined.contains(&id));
        assert_eq!(app.durable.quarantined_panes.get(&id), Some(&original.kind));
        assert!(matches!(app.cards.get(&id), Some(CardRuntime::Loading)));
    }
}

#[test]
fn quarantine_waits_for_genuine_pending_and_delayed_ready_cannot_waive_it() {
    for quarantined_index in [0, 1] {
        let (mut app, cards) = waiting(2);
        let quarantined = cards[quarantined_index];
        let healthy = cards[1 - quarantined_index];
        end_terminals(&mut app, &[quarantined]);
        let pending = Pending {
            admission: false,
            panes: vec![healthy],
        };
        assert_eq!(app.startup_pending(), Ok(Some(pending.clone())));
        let delayed = inspection(&mut app, Ok(StartupState::Ready));
        assert_eq!(delayed.context.pending, pending);
        let observed = Observed {
            context: delayed.context.clone(),
            daemon: delayed.outcome.as_ref().unwrap().clone(),
        };
        assert!(app.startup_observation_current(&observed, &pending));

        let original = app.card_state(quarantined).unwrap().clone();
        app.shared_views.quarantined.insert(quarantined);
        Arc::make_mut(&mut app.durable)
            .quarantined_panes
            .insert(quarantined, original.kind.clone());
        assert_eq!(app.startup_pending(), Ok(Some(pending)));
        assert_eq!(app.startup_pane_check(), Ok(false));
        assert_eq!(reported(&app), None);
        assert!(!app.startup_gate.saving);

        end_terminals(&mut app, &[healthy]);
        let reason = app.startup_pending().unwrap_err();
        assert!(reason.starts_with("recovery-incomplete:"), "{reason}");
        assert!(reason.contains(&quarantined.to_string()), "{reason}");
        drop(app.startup_state_inspected(delayed));
        let reported = reported(&app).expect("open quarantine must answer the launcher");
        assert!(reported.contains("recovery-incomplete:"), "{reported}");
        assert!(reported.contains(&quarantined.to_string()), "{reported}");
        assert!(!app.startup_gate.saving);
        assert!(app.startup_gate.daemon.is_none());
        assert_eq!(app.card_state(quarantined), Some(&original));
        assert!(original.existence.is_open());
        assert!(app.shared_views.quarantined.contains(&quarantined));
        assert_eq!(
            app.durable.quarantined_panes.get(&quarantined),
            Some(&original.kind)
        );
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
            app.startup_state_inspected(old)
        } else {
            let save = app.verify_startup_gate();
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
        assert!(
            app.status
                .as_ref()
                .is_none_or(|status| !status.message.contains(DETAIL))
        );
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
    let save = app.verify_startup_gate();
    drop(app.startup_state_inspected(old));
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
    assert!(reason.contains("workspace storage unavailable"));
    assert!(!reason.contains(DETAIL));
}
