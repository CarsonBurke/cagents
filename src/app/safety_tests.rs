use super::*;
use crate::model::{BootId, HostIncarnation, HostSessionId};

fn live_app() -> (Cagents, CardId, TerminalId, HostSessionProof) {
    let mut app = tests::context_inspector_test_app(true);
    app.context_inspector_open = false;
    let card = app.workspace.focused().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        panic!("terminal fixture");
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let terminal = session.id;
    (app, card, terminal, proof)
}

fn report_close(app: &mut Cagents, outcome: terminal_close::CloseOutcome) {
    let intent = app.terminal_close.as_ref().unwrap().clone();
    let _ = app.update(Message::TerminalCloseFinished {
        card: intent.card,
        terminal: intent.terminal,
        proof: intent.proof,
        generation: intent.generation,
        outcome,
    });
}

fn request_active_close(app: &mut Cagents, card: CardId) {
    let _ = app.update(Message::CloseCard(card));
    report_close(app, terminal_close::CloseOutcome::Active);
}

#[test]
fn pointer_and_keyboard_close_preserve_live_card_until_confirmation() {
    for keyboard in [false, true] {
        let (mut app, card, _, _) = live_app();
        let message = if keyboard {
            close_focused_message(&app.workspace).unwrap()
        } else {
            Message::CloseCard(card)
        };
        let _ = app.update(message);
        assert!(app.terminal_close_dialog().is_none());
        report_close(&mut app, terminal_close::CloseOutcome::Active);
        assert!(app.terminal_close.is_some());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.terminal_close_attempts.is_empty());
        let _ = app.update(Message::CancelTerminalClose);
        assert!(app.terminal_close.is_none());
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(app.terminal_close_attempts.is_empty());
    }
}

#[test]
fn escape_consumes_terminal_confirmation_without_closing_context() {
    let (mut app, card, _, _) = live_app();
    request_active_close(&mut app, card);
    let key = keyboard::Key::Named(keyboard::key::Named::Escape);
    let _ = app.update(Message::KeyEvent(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    }));
    assert!(app.terminal_close.is_none());
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.terminal_close_attempts.is_empty());
    assert_eq!(app.workspace.focused(), Some(card));
}

#[test]
fn exit_during_confirmation_removes_without_shutdown_and_proof_change_keeps_card() {
    let (mut app, card, _, _) = live_app();
    request_active_close(&mut app, card);
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(app.terminal_close.is_none());
    assert!(app.closed_terminal_cards.contains(&card));
    assert!(app.terminal_close_attempts.is_empty());

    let (mut app, card, _, _) = live_app();
    request_active_close(&mut app, card);
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    if let TerminalContinuity::Live {
        host_incarnation, ..
    } = &mut session.continuity
    {
        *host_incarnation = HostIncarnation::new("restarted");
    }
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(app.terminal_close.as_ref().unwrap().detail.is_some());
    assert!(app.terminal_close_attempts.is_empty());
    assert!(app.card_state(card).unwrap().existence.is_open());
}

#[test]
fn unreachable_and_duplicate_completions_never_remove_or_resend() {
    let (mut app, card, terminal, proof) = live_app();
    let _ = app.update(Message::CloseCard(card));
    let _pending = app.update(Message::ConfirmTerminalClose); // Not polled: never touches a real daemon.
    let _duplicate = app.update(Message::ConfirmTerminalClose);
    assert_eq!(app.terminal_close_attempts.len(), 1);
    let generation = app.terminal_close.as_ref().unwrap().generation;
    let _ = app.update(Message::TerminalCloseFinished {
        card,
        terminal,
        proof: proof.clone(),
        generation,
        outcome: terminal_close::CloseOutcome::Unreachable("disconnected".into()),
    });
    let _ = app.update(Message::TerminalCloseFinished {
        card,
        terminal,
        proof,
        generation,
        outcome: terminal_close::CloseOutcome::Closed,
    });
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(!app.closed_terminal_cards.contains(&card));
    assert_eq!(app.terminal_close_attempts.len(), 1);
}

#[test]
fn every_degraded_github_refresh_revokes_prior_dialog_authority() {
    for unavailable in [
        GitHubAvailability::MissingCli,
        GitHubAvailability::Unauthenticated,
        GitHubAvailability::NotAssociated,
        GitHubAvailability::Loading,
        GitHubAvailability::Failed {
            detail: "offline".into(),
        },
    ] {
        let mut app = tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let key = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        app.repository_generations.insert(project, 1);
        app.github_availability.insert(
            project,
            GitHubAvailability::Ready {
                repository_key: key.clone(),
                pull_requests: Vec::new(),
                refreshed_at: 1,
            },
        );
        let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
            project_id: project,
            worktree: None,
        }));
        app.pull_request_dialog.as_mut().unwrap().head = "topic".into();
        app.pull_request_dialog.as_mut().unwrap().title = "Authored title".into();
        app.github_availability.insert(project, unavailable);
        let _ = app.update(Message::SubmitPullRequest);
        assert!(
            app.pull_request_dialog.is_some(),
            "stale draft was not submitted"
        );
        let _ = app.update(Message::CancelDialog);
        let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
            project_id: project,
            worktree: None,
        }));
        assert!(
            app.pull_request_dialog.is_none(),
            "degraded identity cannot open a mutation"
        );
    }
}

#[test]
fn refresh_start_and_stale_completion_cannot_reauthorize() {
    let mut app = tests::context_inspector_test_app(false);
    let project = ProjectId::new();
    app.repository_generations.insert(project, 4);
    app.github_availability.insert(
        project,
        GitHubAvailability::Ready {
            repository_key: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
            pull_requests: Vec::new(),
            refreshed_at: 0,
        },
    );
    let _unpolled = app.repository_load_task(PathBuf::from("/unaccessed"), Some(project), false);
    assert!(matches!(
        app.github_availability.get(&project),
        Some(GitHubAvailability::Loading)
    ));
    let _ = app.update(Message::RepositoryLoaded {
        open_cards: false,
        project_id: project,
        generation: 4,
        result: Err("old failure".into()),
    });
    assert!(matches!(
        app.github_availability.get(&project),
        Some(GitHubAvailability::Loading)
    ));
}

#[test]
fn pending_spawn_close_does_not_remove_card_or_reject_future_completion() {
    let mut app = tests::context_inspector_test_app(true);
    app.context_inspector_open = false;
    let card = app.workspace.focused().unwrap();
    app.cards.insert(card, CardRuntime::Loading);
    for message in [Message::CloseCard(card), Message::CommitClose(card)] {
        let _ = app.update(message);
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(app.workspace.order().any(|id| id == card));
        assert!(app.terminal_close_attempts.is_empty());
    }
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    let prior_terminal = session.id;
    session.continuity = TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    app.cards
        .insert(card, CardRuntime::Failed("previous terminal exited".into()));
    let _unpolled_spawn = app.update(Message::RetryCard(card));
    let CardKind::Terminal(replacement) = &app.card_state(card).unwrap().kind else {
        unreachable!()
    };
    assert_ne!(replacement.id, prior_terminal);
    assert!(matches!(
        replacement.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: Some(_),
            ..
        }
    ));
    let _ = app.update(Message::CloseCard(card));
    let _ = app.update(Message::CommitClose(card));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert!(app.terminal_close_attempts.is_empty());
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    let _ = app.update(Message::CloseCard(card));
    assert!(app.terminal_close.is_some());
    assert_eq!(app.terminal_close_attempts.len(), 1);
}

#[test]
fn terminal_confirmation_owns_tab_navigation_and_restores_shell_setting() {
    let (mut app, card, _, _) = live_app();
    app.core.set_keyboard_nav(true);
    request_active_close(&mut app, card);
    assert!(!app.core.keyboard_nav());
    let _ = app.cycle_terminal_close_focus(false);
    assert!(!app.core.keyboard_nav());
    let _ = app.update(Message::CancelTerminalClose);
    assert!(app.core.keyboard_nav());
}

#[test]
fn idle_inspection_closes_without_confirmation_and_stale_results_cannot_close_replacement() {
    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    assert!(app.terminal_close_dialog().is_none());
    report_close(&mut app, terminal_close::CloseOutcome::Closed);
    assert!(app.terminal_close.is_none());
    assert!(app.closed_terminal_cards.contains(&card));

    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    let old = app.terminal_close.as_ref().unwrap().clone();
    report_close(&mut app, terminal_close::CloseOutcome::Active);
    let _ = app.update(Message::CancelTerminalClose);
    let _ = app.update(Message::CloseCard(card));
    let _ = app.update(Message::TerminalCloseFinished {
        card,
        terminal: old.terminal,
        proof: old.proof,
        generation: old.generation,
        outcome: terminal_close::CloseOutcome::Closed,
    });
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(!app.closed_terminal_cards.contains(&card));
    assert!(app.terminal_close.as_ref().unwrap().pending);
}

#[test]
fn terminal_close_opt_out_persists_only_on_close_and_can_be_reenabled() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let (mut app, card, _, _) = live_app();
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    request_active_close(&mut app, card);
    let _ = app.update(Message::TerminalCloseDontAskChanged(true));
    let _ = app.update(Message::CancelTerminalClose);
    assert!(app.settings.confirm_terminal_close);
    assert!(!path.exists());
    request_active_close(&mut app, card);
    let _ = app.update(Message::TerminalCloseDontAskChanged(true));
    // The process may finish while its confirmation is open. Keep this
    // preference test independent of any real daemon or process shutdown.
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    let close = app.update(Message::ConfirmTerminalClose);
    tests::settle_settings_task(&mut app, close);
    assert!(!settings::load(&path).unwrap().confirm_terminal_close);
    let enable = app.update(Message::SettingsChanged {
        field: SettingsField::ConfirmTerminalClose,
        value: "true".into(),
    });
    tests::settle_settings_task(&mut app, enable);
    assert!(settings::load(&path).unwrap().confirm_terminal_close);
}
