//! Real private window sockets feed the production event mapper and App update.
//! Provider/terminal attachment tasks remain unpolled; this is not native UX.
use super::*;
use crate::agent_execution::{Agent, HostedSession, Surface};
use crate::app::workspace_model_sync::ModelUpdate;

fn fixture(native: bool) -> (Cagents, Agent, instance::TrayActivation) {
    let (mut app, mut agent) = super::tests::scheduled_attention_fixture();
    app.workspace_model
        .projects
        .push(crate::workspace_model::ModelProject {
            repository: "/tmp".into(),
            name: "Scheduled".into(),
            kind: Default::default(),
            ordinal: 0,
        });
    if native {
        let console = agent.console.as_ref().unwrap().clone();
        agent.spec.surface = Surface::Chat;
        agent.session = Some(HostedSession::Chat(crate::chat::Session {
            id: console.id,
            proof: console.continuity.live_proof(),
            terminal: Some(console.clone()),
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: agent.spec.provider,
                session_id: Some("existing-tray-thread".into()),
                recipe: console.launch,
            },
        }));
    }
    let source = activation_session(&agent).unwrap();
    let mut sources = vec![instance::tray_source_fingerprint(&source).unwrap()];
    if native {
        sources.push(
            instance::tray_source_fingerprint(&HostedSession::Terminal(
                agent.console.clone().unwrap(),
            ))
            .unwrap(),
        );
    }
    let intent = instance::TrayActivation {
        request: uuid::Uuid::new_v4(),
        registry: app.workspace_model.identity,
        host: agent
            .console
            .as_ref()
            .unwrap()
            .continuity
            .live_proof()
            .unwrap(),
        window: instance::window_slot().into(),
        session: source.id(),
        sources,
    };
    (app, agent, intent)
}

fn deliver(app: &mut Cagents, intent: &instance::TrayActivation) {
    let event = instance::fixture_activation_event(intent, false);
    assert!(event.session.is_none());
    assert_eq!(event.tray.as_ref(), Some(intent));
    drop(app.update(super::super::ui_activation_message(event)));
}

fn assert_exact_pane(app: &Cagents, agent: &Agent, intent: &instance::TrayActivation) {
    let card = CardId::from_uuid(agent.id.as_uuid());
    assert_eq!(app.workspace.focused(), Some(card));
    assert_eq!(app.durable.active_channel, agent.channel());
    assert_eq!(app.durable.cards.len(), 1);
    let source = match &app.card_state(card).unwrap().kind {
        CardKind::Chat(source) => {
            assert!(
                app.chat_start_matches(card, source),
                "attach uses the existing exact descriptor"
            );
            HostedSession::Chat(source.clone())
        }
        CardKind::Terminal(source) => {
            assert!(!app.chat_start_pending(card));
            HostedSession::Terminal(source.clone())
        }
        _ => panic!("the requested existing session owns the pane"),
    };
    assert!(intent.source_matches(&source));
    assert!(app.notification_ui.session_activation.is_none());
}

#[test]
fn tray_socket_event_opens_exact_headless_native_and_console_without_saved_cards() {
    for native in [false, true] {
        let (mut app, agent, intent) = fixture(native);
        app.workspace_model.agents.push(agent.clone());
        drop(app.update(Message::NotificationUi(Action::HostBound(
            intent.host.clone(),
        ))));
        assert!(app.durable.cards.is_empty());
        deliver(&mut app, &intent);
        assert_exact_pane(&app, &agent, &intent);
        let generation = app.notification_ui.activation_generation;
        deliver(&mut app, &intent);
        assert_eq!(
            app.notification_ui.activation_generation, generation,
            "socket retry cannot replay an admitted request"
        );
        assert_exact_pane(&app, &agent, &intent);
        let event = instance::fixture_activation_event(&intent, true);
        assert!(event.tray.is_none());
        assert!(
            matches!(super::super::ui_activation_message(event), Message::ActivateSession { session, .. } if session == intent.session),
            "ordinary links keep their existing saved-conversation route"
        );
    }
}

#[test]
fn tray_socket_cold_intent_waits_for_matching_host_and_model_without_unrelated_rescans() {
    for native in [false, true] {
        let (mut app, agent, intent) = fixture(native);
        let mut model = app.workspace_model.clone();
        model.agents.push(agent.clone());
        model.sequence += 1;
        app.workspace_model = crate::workspace_model::WorkspaceModel::default();
        deliver(&mut app, &intent);
        assert_eq!(app.notification_ui.session_activation, Some(intent.session));
        assert!(app.durable.cards.is_empty());
        drop(app.update(Message::NotificationUi(Action::HostBound(
            intent.host.clone(),
        ))));
        assert!(app.durable.cards.is_empty());
        let mut empty = model.clone();
        empty.agents.clear();
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                empty,
            ))),
        );
        let checks = app.notification_ui.tray_source_checks;
        for _ in 0..100 {
            drop(app.update(Message::NotificationUi(Action::ReviewsReported(Ok(())))));
        }
        assert_eq!(
            app.notification_ui.tray_source_checks, checks,
            "unrelated App wakes cannot rescan/hash a pending tray target"
        );
        model.sequence += 1;
        drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))));
        assert_exact_pane(&app, &agent, &intent);
    }
}

#[test]
fn tray_nonce_supersedes_late_expiry_duplicate_delivery_and_inbox_resolution() {
    let (mut app, agent, old) = fixture(false);
    drop(app.update(Message::NotificationUi(Action::HostBound(old.host.clone()))));
    deliver(&mut app, &old);
    let old_generation = app.notification_ui.activation_generation;
    let mut current = old.clone();
    current.request = uuid::Uuid::new_v4();
    let operation = uuid::Uuid::new_v4();
    let mut stale_console = agent.console.clone().unwrap();
    stale_console.id = TerminalId::new();
    let lineage = crate::agent_execution::Lineage(stale_console.id);
    let mut stale_card = super::super::tests::terminal_card_state();
    stale_card.kind = CardKind::Terminal(stale_console.clone());
    let stale_card_id = stale_card.id;
    Arc::make_mut(&mut app.durable).cards.push(stale_card);
    app.notification_ui.inbox_activation = Some((lineage, operation));
    deliver(&mut app, &current);
    assert!(
        app.notification_ui.inbox_activation.is_none(),
        "the new tray request must retire an older inbox operation before its completion"
    );
    let generation = app.notification_ui.activation_generation;
    drop(
        app.update(Message::NotificationUi(Action::TrayActivationExpired {
            request: old.request,
            generation: old_generation,
        })),
    );
    drop(
        app.update(Message::NotificationUi(Action::SessionActivationExpired {
            session: old.session,
            generation: old_generation,
        })),
    );
    drop(app.update(Message::NotificationUi(Action::InboxResolved {
        lineage,
        operation,
        result: Ok(stale_console),
    })));
    deliver(&mut app, &old);
    assert_eq!(app.notification_ui.activation_generation, generation);
    assert_eq!(
        app.notification_ui.session_activation,
        Some(current.session)
    );
    assert_eq!(
        app.notification_ui.tray_activation.as_ref().unwrap().intent,
        current
    );
    assert_ne!(app.workspace.focused(), Some(stale_card_id));
    assert_eq!(app.durable.cards.len(), 1);
    Arc::make_mut(&mut app.durable)
        .cards
        .retain(|card| card.id != stale_card_id);
    let mut model = app.workspace_model.clone();
    model.agents.push(agent.clone());
    model.sequence += 1;
    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))));
    assert_exact_pane(&app, &agent, &current);
}

#[test]
fn tray_socket_refuses_replaced_registry_host_process_duplicate_closed_and_deleted_targets() {
    for native in [false, true] {
        for mutation in 0..8 {
            let (mut app, agent, intent) = fixture(native);
            app.workspace_model.agents.push(agent.clone());
            drop(app.update(Message::NotificationUi(Action::HostBound(
                intent.host.clone(),
            ))));
            app.settings_reset_confirm = true;
            deliver(&mut app, &intent);
            assert_eq!(app.notification_ui.session_activation, Some(intent.session));
            assert!(app.durable.cards.is_empty());
            app.settings_reset_confirm = false;
            let mut model = app.workspace_model.clone();
            model.sequence += 1;
            match mutation {
                0 => model.identity = uuid::Uuid::new_v4(),
                1 => {
                    let mut replacement = intent.host.clone();
                    replacement.host_incarnation = "replaced-host".into();
                    drop(app.update(Message::NotificationUi(Action::HostBound(replacement))));
                }
                2 => match &mut model.agents[0].session {
                    Some(HostedSession::Chat(source)) => source
                        .launch
                        .recipe
                        .arguments
                        .push("--different-process".into()),
                    _ => model.agents[0]
                        .console
                        .as_mut()
                        .unwrap()
                        .launch
                        .arguments
                        .push("--different-process".into()),
                },
                3 => model.agents[0].closed = true,
                4 => {
                    let mut duplicate = agent.clone();
                    duplicate.id = crate::model::AgentId::new();
                    model.agents.push(duplicate);
                }
                5 => model.closed_projects.push("/tmp".into()),
                7 => {
                    // Simulate a captured origin belonging to another backend;
                    // no global device or authority environment is mutated.
                    app.notification_ui
                        .tray_activation
                        .as_mut()
                        .unwrap()
                        .backend = Some((
                        "different-device".into(),
                        "different-host".into(),
                        "different-executable".into(),
                    ));
                }
                6 => {
                    let mut closed = super::super::tests::terminal_card_state();
                    closed.id = CardId::from_uuid(agent.id.as_uuid());
                    closed.kind = match activation_session(&agent).unwrap() {
                        HostedSession::Chat(source) => CardKind::Chat(source),
                        HostedSession::Terminal(source) => CardKind::Terminal(source),
                    };
                    closed.existence =
                        super::super::project_ui::removed_existence("closed before tray admission");
                    Arc::make_mut(&mut app.durable).cards.push(closed);
                }
                _ => unreachable!(),
            }
            drop(
                app.update(Message::WorkspaceModelObserved(if mutation == 0 {
                    ModelUpdate::Connected(model)
                } else {
                    ModelUpdate::Read(model)
                })),
            );
            assert!(
                app.workspace.focused().is_none(),
                "mutation {mutation} cannot focus a replacement"
            );
            assert!(
                !app.durable
                    .cards
                    .iter()
                    .any(|card| card.existence.is_open()),
                "mutation {mutation} cannot mount a provider"
            );
            assert!(
                app.notification_ui.session_activation.is_none(),
                "mutation {mutation} retires this exact stale tray intent"
            );
        }
    }
}

#[test]
fn tray_source_metadata_enrichment_and_modal_deferral_preserve_the_exact_process() {
    for native in [false, true] {
        let (mut app, agent, intent) = fixture(native);
        app.workspace_model.agents.push(agent.clone());
        drop(app.update(Message::NotificationUi(Action::HostBound(
            intent.host.clone(),
        ))));
        app.settings_reset_confirm = true;
        deliver(&mut app, &intent);
        assert_eq!(app.notification_ui.session_activation, Some(intent.session));
        assert!(app.durable.cards.is_empty());
        let mut model = app.workspace_model.clone();
        model.sequence += 1;
        match &mut model.agents[0].session {
            Some(HostedSession::Chat(source)) => {
                source.launch.session_id = Some("enriched-provider-thread".into())
            }
            _ => {
                model.agents[0].console.as_mut().unwrap().resume_hint =
                    Some("enriched-title".into())
            }
        }
        let enriched = model.agents[0].clone();
        drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))));
        assert!(
            app.durable.cards.is_empty(),
            "model arrival cannot bypass a modal"
        );
        app.settings_reset_confirm = false;
        drop(app.update(Message::NotificationUi(Action::ReviewsReported(Ok(())))));
        assert_exact_pane(&app, &enriched, &intent);
    }
}

#[test]
fn tray_socket_generic_session_keeps_closed_saved_resume() {
    const CHILD: &str = "CAGENTS_TRAY_SAVED_LINK_TEST_CHILD";
    if std::env::var(CHILD).as_deref() != Ok("1") {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        for leaf in ["home", "data", "config", "state", "cache", "runtime", "tmp"] {
            let path = root.path().join(leaf);
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // The unchanged saved-link lookup reads other saved-window paths.
        // Isolate those paths in a fresh process; preserve inherited authority.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "app::notification_ui::tray_tests::tray_socket_generic_session_keeps_closed_saved_resume", "--nocapture"])
            .env(CHILD, "1")
            .env("HOME", root.path().join("home"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
            .env("TMPDIR", root.path().join("tmp"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "isolated saved-link child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    use cosmic::iced::futures::StreamExt;
    let directory = tempfile::tempdir().unwrap();
    let mut app = super::super::tests::context_inspector_test_app(false);
    app.context_inspector_open = false;
    let mut source = crate::chat::fresh_session(
        crate::agent::Provider::Claude,
        TerminalId::new(),
        directory.path().into(),
        &crate::account::Account::system(crate::agent::Provider::Claude),
    )
    .unwrap();
    source.launch.session_id = Some("closed-saved-tray-link-control".into());
    let mut card = super::super::tests::terminal_card_state();
    card.title = "Closed saved link control".into();
    card.kind = CardKind::Chat(source.clone());
    card.existence =
        super::super::project_ui::removed_existence("saved link remains intentionally resumable");
    Arc::make_mut(&mut app.durable).cards.push(card);
    let (_, _, mut route) = fixture(false);
    route.session = source.id;
    let event = instance::fixture_activation_event(&route, true);
    let message = super::super::ui_activation_message(event);
    assert!(matches!(&message, Message::ActivateSession { session, .. } if *session == source.id));
    let task = app.update(message);
    let resolved = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut events = iced::runtime::task::into_stream(task).unwrap();
                while let Some(event) = events.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                        super::super::chat_ui::Action::SessionResolved(result),
                    ))) = event
                    {
                        return result;
                    }
                }
                panic!("saved-link lookup must publish its actual resolution");
            })
            .await
            .unwrap()
        });
    assert!(
        resolved.is_ok(),
        "the real generic saved-link lookup must succeed"
    );
    // Dispatch the real lookup result, but leave provider attachment unpolled.
    drop(app.update(Message::ChatUi(
        super::super::chat_ui::Action::SessionResolved(resolved),
    )));
    let opened = app.card_state(app.workspace.focused().unwrap()).unwrap();
    assert!(opened.existence.is_open());
    let CardKind::Chat(resumed) = &opened.kind else {
        panic!("saved chat resumes as chat");
    };
    assert_ne!(resumed.id, source.id);
    assert_eq!(resumed.launch, source.launch);
    assert!(app.chat_start_pending(opened.id));
    assert!(app.notification_ui.tray_activation.is_none());
}
