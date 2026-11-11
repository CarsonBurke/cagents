use super::*;

fn initial_chat() -> (Cagents, CardId, chat::Session) {
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let session = chat::Session {
        id: TerminalId::new(),
        proof: Some(HostSessionProof {
            host_session_id: "initial".into(),
            boot_id: "boot".into(),
            host_incarnation: "source".into(),
        }),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
        launch: chat::Launch {
            provider: Provider::Codex,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            recipe: LaunchRecipe {
                program: "never-execute-startup-provider".into(),
                arguments: vec![],
                working_directory: "/".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
        },
    };
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(session.clone());
    app.startup_gate.initial_panes.capture(
        instance::WindowRestorePolicy::ExistingOnly,
        &app.durable.cards,
    );
    (app, card, session)
}

fn scheduled(app: &Cagents, card: CardId) -> (Target, chat::Intent) {
    let Phase::Starting(attempt) = &app.chat_ui.phases[&card] else {
        panic!("expected actual scheduled attachment")
    };
    (attempt.target, attempt.intent)
}

#[test]
fn initial_chat_intent_is_captured_before_async_work_and_not_rearmed_after_current_result() {
    let (mut app, card, session) = initial_chat();
    // Global transport admission is ordinary in this test. The exact initial
    // pane key, rather than worker timing, captures attachment-only intent.
    drop(app.start_chat(card, session.clone(), chat::Intent::Restore, None, false));
    let (target, intent) = scheduled(&app, card);
    assert_eq!(intent, chat::Intent::RestoreExisting);
    let stale = Target {
        generation: target.generation + 1,
        ..target
    };
    drop(app.handle_chat_ui(Action::Ready(
        stale,
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            "stale",
        )))))),
    )));
    assert!(
        app.startup_gate
            .initial_panes
            .existing_only(card, session.id)
    );
    drop(app.handle_chat_ui(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            "exact owner unavailable",
        )))))),
    )));
    assert!(
        !app.startup_gate
            .initial_panes
            .existing_only(card, session.id)
    );
    assert_eq!(
        intent,
        chat::Intent::RestoreExisting,
        "an already-issued request cannot become a provider start"
    );
    drop(app.start_chat(card, session.clone(), chat::Intent::Restore, None, false));
    assert_eq!(scheduled(&app, card).1, chat::Intent::Restore);
    app.startup_gate.initial_panes.capture(
        instance::WindowRestorePolicy::ExistingOnly,
        &app.durable.cards,
    );
    assert!(
        !app.startup_gate
            .initial_panes
            .existing_only(card, session.id)
    );
}

#[test]
fn explicit_chat_actions_retire_only_their_initial_pane_and_new_panes_restore_normally() {
    for intent in [
        chat::Intent::Start,
        chat::Intent::Resume,
        chat::Intent::RetryStart,
    ] {
        let (mut app, card, session) = initial_chat();
        drop(app.start_chat(card, session.clone(), intent, None, false));
        assert_eq!(scheduled(&app, card).1, intent);
        assert!(
            !app.startup_gate
                .initial_panes
                .existing_only(card, session.id)
        );
    }
    let (mut app, original_card, original) = initial_chat();
    let mut later = app.card_state(original_card).unwrap().clone();
    later.id = CardId::new();
    let mut new_session = original.clone();
    new_session.id = TerminalId::new();
    later.kind = CardKind::Chat(new_session.clone());
    let later_card = later.id;
    Arc::make_mut(&mut app.durable).cards.push(later);
    drop(app.start_chat(later_card, new_session, chat::Intent::Restore, None, false));
    assert_eq!(scheduled(&app, later_card).1, chat::Intent::Restore);
    assert!(
        app.startup_gate
            .initial_panes
            .existing_only(original_card, original.id)
    );
}
