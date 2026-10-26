use super::*;

fn hosted_omp() -> CardState {
    let mut card = super::super::tests::terminal_card_state();
    card.exposure = Exposure::Offscreen;
    card.residency = Residency::Hibernated;
    let CardKind::Terminal(session) = &mut card.kind else {
        unreachable!()
    };
    session.launch.program = "/usr/bin/omp".into();
    session.launch.arguments.clear();
    session.launch.working_directory = "/fixture/background".into();
    session.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("background-owner"),
        boot_id: crate::model::BootId::new("background-boot"),
        host_incarnation: crate::model::HostIncarnation::new("background-incarnation"),
    };
    card
}

#[test]
fn restored_omp_attaches_for_background_metadata_without_opening_or_replaying() {
    let mut app = super::super::tests::context_inspector_test_app(false);
    let original = hosted_omp();
    let CardKind::Terminal(session) = &original.kind else {
        unreachable!()
    };
    app.durable = Arc::new(WorkspaceState {
        cards: vec![original.clone()],
        ..Default::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    drop(app.initialize_runtimes());
    assert_eq!(app.terminal_starts_in_flight(), 1);
    assert!(app.terminal_start_in_flight(original.id));
    assert_eq!(
        app.lifecycle.state(original.id),
        Some((&Exposure::Offscreen, &Residency::Hibernated))
    );
    assert!(matches!(
        app.cards.get(&original.id),
        Some(CardRuntime::Loading)
    ));
    assert_eq!(app.workspace.focused(), None);
    assert_eq!(app.card_state(original.id).unwrap().kind, original.kind);

    // The existing attach-only reducer must not relaunch a session that no
    // longer exists. Its original identity remains for a later explicit open.
    let notices = app.notices.entries.len();
    drop(app.update(Message::RestoredAgentAttached {
        card: original.id,
        terminal: session.id,
        result: Ok(None),
    }));
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert!(app.restored_agent_attaches.is_empty());
    assert_eq!(app.card_state(original.id).unwrap().kind, original.kind);
    assert_eq!(app.workspace.focused(), None);
    assert_eq!(app.notices.entries.len(), notices);
}

#[test]
fn newly_adopted_omp_uses_the_hidden_attachment_without_changing_scope_or_focus() {
    let mut app = super::super::tests::context_inspector_test_app(true);
    app.active_workspace = Some("/fixture/control".into());
    app.known_worktree_roots
        .insert("/fixture/background".into());
    let focused = app.workspace.focused();
    let strip = app.workspace.order().collect::<Vec<_>>();
    let source = hosted_omp();
    let CardKind::Terminal(session) = source.kind else {
        unreachable!()
    };
    let mut descriptor = crate::terminal::ipc::TerminalDescriptor::relaunchable(
        session.id,
        session.launch.clone(),
        None,
    );
    descriptor.session = session.clone();
    drop(app.update(Message::HostedTerminalsObserved(vec![descriptor])));
    let cards = app
        .durable
        .cards
        .iter()
        .filter(|card| matches!(&card.kind, CardKind::Terminal(saved) if saved.id == session.id))
        .collect::<Vec<_>>();
    assert_eq!(cards.len(), 1);
    let card = cards[0].id;
    assert_eq!(cards[0].kind, CardKind::Terminal(session));
    assert_eq!(cards[0].exposure, Exposure::Offscreen);
    assert_eq!(cards[0].residency, Residency::Hibernated);
    assert_eq!(app.terminal_starts_in_flight(), 1);
    assert!(app.terminal_start_in_flight(card));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert_eq!(
        app.active_workspace.as_deref(),
        Some(Path::new("/fixture/control"))
    );
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), strip);
    assert!(!app.durable.card_scopes.contains_key(&card));
}

#[test]
fn background_observation_requires_a_live_recognized_provider_not_a_shell_title() {
    let card = hosted_omp();
    let CardKind::Terminal(mut session) = card.kind else {
        unreachable!()
    };
    assert!(observes_terminal_agent(&session));
    let omp = session.launch.clone();
    session.launch.program = "/bin/bash".into();
    session.launch.arguments = vec!["-c".into(), "omp".into()];
    assert!(!observes_terminal_agent(&session));
    session.resume_hint = crate::model::agent_recovery::observed_hint_from_launch(&omp);
    assert!(session.resume_hint.is_some());
    assert!(observes_terminal_agent(&session));
    session.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: session.launch.clone(),
        last_exit: None,
    };
    assert!(!observes_terminal_agent(&session));
}
