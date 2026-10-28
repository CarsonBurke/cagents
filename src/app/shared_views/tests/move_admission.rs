use super::*;
use crate::app::workspace_model_sync::ModelUpdate;

const SAVED_MOVE_NOTICE: &str = "This saved pane has not been reopened. Reconnect the workspace to reopen it, or open the conversation from history. Saved text stays in this pane.";
const UNNAMED_MOVE_NOTICE: &str = "This saved pane has not been reopened. Reconnect the workspace to reopen it. You can copy saved text into a new conversation.";
const ATTACHMENT_MOVE_NOTICE: &str = "This saved pane has not been reopened. Reconnect the workspace to reopen this pane with its saved attachments. Copying text or opening conversation history will not include these attachments.";
const SCOPE_MOVE_NOTICE: &str =
    "Wait for this pane to finish joining the shared workspace before moving it to a channel.";
const MOVE_CLOSE_NOTICE: &str =
    "This pane is moving to a channel. Wait for the move to finish before closing it.";
const DRAFT: &str = "  keep this saved draft 🦀\n";

fn has_notice(app: &Cagents, expected: &str) -> bool {
    app.status
        .as_ref()
        .is_some_and(|notice| notice.message == expected)
        || app
            .notices
            .recent()
            .any(|entry| entry.notice.message == expected)
}

fn awaiting_fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
    let (mut app, source, destination, pane) = retained_chat_move_fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, DRAFT.into())]));
    // A typed saved-descriptor fixture, hydrated through the actual startup
    // path. Its cached shared membership is not provider admission. No returned
    // Task is polled and no provider, daemon or saved-session writer is used.
    drop(app.initialize_runtimes());
    assert!(app.chat_awaiting_admission(pane.id));
    assert!(!app.chat_start_pending(pane.id));
    assert_eq!(saved_text(&app, pane.id), DRAFT);
    (app, source, destination, pane)
}

fn saved_text(app: &Cagents, card: CardId) -> String {
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("saved runtime expected");
    };
    runtime.unsent_text()
}

fn runtime_allocation(app: &Cagents, card: CardId) -> usize {
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("saved runtime expected");
    };
    &**runtime as *const _ as usize
}

#[test]
fn saved_chat_cannot_move_during_seed_and_still_closes_without_provider_admission() {
    let (mut app, source, destination, pane) = awaiting_fixture();
    let CardKind::Chat(saved) = &pane.kind else {
        unreachable!();
    };
    let scope = Scope::Channel(source);
    app.shared_views.initialized.remove(&scope);
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    app.persist();
    let ModelEdit::SeedView {
        scope: actual,
        panes,
        ..
    } = rx.try_recv().unwrap()
    else {
        panic!("actual persist must seed the saved source scope");
    };
    assert_eq!(actual, scope);
    assert_eq!(panes, [pane.clone()]);
    assert!(app.shared_views.seeding.contains(&scope));
    assert!(rx.try_recv().is_err());
    let allocation = runtime_allocation(&app, pane.id);
    assert!(!app.pane_has_move_destination(pane.id));
    assert!(app.pane_move_destinations(pane.id).is_empty());
    assert!(!app.can_move_pane_to_channel(pane.id, destination));
    drop(app.update(Message::MovePaneToChannel {
        card: pane.id,
        channel: destination,
    }));
    assert!(has_notice(&app, SAVED_MOVE_NOTICE));
    assert!(!app.pane_transfer_pending(pane.id));
    assert!(rx.try_recv().is_err());
    assert_eq!(runtime_allocation(&app, pane.id), allocation);
    assert_eq!(saved_text(&app, pane.id), DRAFT);
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert_eq!(
        app.durable.pending_chat_admissions.get(&pane.id),
        Some(saved)
    );

    drop(app.update(Message::CloseCard(pane.id)));
    assert!(
        !app.card_state(pane.id)
            .is_some_and(|card| card.existence.is_open())
    );
    assert!(!app.durable.pending_chat_admissions.contains_key(&pane.id));
    assert!(!app.chat_start_pending(pane.id));
    assert!(app.workspace.card(pane.id).is_none());
    assert!(!app.pane_transfer_pending(pane.id));
    assert!(rx.try_recv().is_err(), "no Transfer was ever queued");
    // Later closed-card canonical projection is the separate #1658 contract.
    // This test neither fabricates an ACK for the refused Move nor changes it.
}

#[test]
fn an_unstarted_saved_chat_gets_copy_guidance_instead_of_unavailable_history() {
    let (mut app, _, destination, pane) = awaiting_fixture();
    let session = crate::chat::fresh_session(
        crate::agent::Provider::Claude,
        TerminalId::new(),
        PathBuf::from("/repo"),
        &crate::account::Account::system(crate::agent::Provider::Claude),
    )
    .unwrap();
    assert!(session.is_unstarted());
    assert!(session.launch.session_id.is_none());
    let kind = CardKind::Chat(session.clone());
    // A typed unnamed saved descriptor uses the same startup admission path.
    // History has no provider session ID to index; no provider is executed.
    app.card_state_mut(pane.id).unwrap().kind = kind.clone();
    app.workspace_model
        .views
        .iter_mut()
        .flat_map(|view| &mut view.panes)
        .find(|candidate| candidate.id == pane.id)
        .unwrap()
        .kind = kind.clone();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, DRAFT.into())]));
    drop(app.initialize_runtimes());
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.update(Message::MovePaneToChannel {
        card: pane.id,
        channel: destination,
    }));
    assert!(has_notice(&app, UNNAMED_MOVE_NOTICE));
    assert!(!has_notice(&app, SAVED_MOVE_NOTICE));
    assert!(app.chat_awaiting_admission(pane.id));
    assert!(!app.chat_start_pending(pane.id));
    assert!(!app.pane_transfer_pending(pane.id));
    assert_eq!(app.card_state(pane.id).unwrap().kind, kind);
    assert_eq!(saved_text(&app, pane.id), DRAFT);
    assert_eq!(
        app.durable.pending_chat_admissions.get(&pane.id),
        Some(&session)
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn an_image_only_unstarted_saved_chat_keeps_its_exact_source_when_move_is_refused() {
    crate::chat::attachment_assets::with_test_image(|image| {
        let (mut app, _, destination, pane) = awaiting_fixture();
        let session = crate::chat::fresh_session(
            crate::agent::Provider::Claude,
            TerminalId::new(),
            PathBuf::from("/repo"),
            &crate::account::Account::system(crate::agent::Provider::Claude),
        )
        .unwrap();
        assert!(session.is_unstarted());
        let kind = CardKind::Chat(session.clone());
        app.card_state_mut(pane.id).unwrap().kind = kind.clone();
        app.workspace_model
            .views
            .iter_mut()
            .flat_map(|view| &mut view.panes)
            .find(|candidate| candidate.id == pane.id)
            .unwrap()
            .kind = kind.clone();
        // A referenced image-only saved draft enters the real startup runtime
        // path. No provider, clipboard or session-writer Task is executed.
        app.chat_ui = chat_ui::State::with_draft_attachments(
            HashMap::new(),
            HashMap::from([(pane.id, vec![image.clone()])]),
        );
        drop(app.initialize_runtimes());
        assert!(app.chat_awaiting_admission(pane.id));
        assert!(saved_text(&app, pane.id).is_empty());
        let allocation = runtime_allocation(&app, pane.id);
        let before = app.persistent_workspace();
        assert_eq!(before.chat_draft_attachments[&pane.id], [image.clone()]);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.update(Message::MovePaneToChannel {
            card: pane.id,
            channel: destination,
        }));
        assert!(has_notice(&app, ATTACHMENT_MOVE_NOTICE));
        assert!(!has_notice(&app, UNNAMED_MOVE_NOTICE));
        assert!(app.chat_awaiting_admission(pane.id));
        assert!(!app.chat_start_pending(pane.id));
        assert!(!app.pane_transfer_pending(pane.id));
        assert!(app.pane_move_destinations(pane.id).is_empty());
        assert_eq!(runtime_allocation(&app, pane.id), allocation);
        assert_eq!(app.card_state(pane.id).unwrap().kind, kind);
        assert_eq!(
            app.durable.pending_chat_admissions.get(&pane.id),
            Some(&session)
        );
        let after = app.persistent_workspace();
        assert_eq!(after.chat_draft_attachments[&pane.id], [image]);
        assert_eq!(after.pending_chat_admissions[&pane.id], session);
        assert!(rx.try_recv().is_err());
    });
}

#[test]
fn source_scope_admission_gates_both_public_move_and_drop() {
    for seeding in [false, true] {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let scope = Scope::Channel(source);
        app.shared_views.initialized.remove(&scope);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        if seeding {
            app.persist();
            assert!(matches!(
                rx.try_recv().unwrap(),
                ModelEdit::SeedView { scope: actual, .. } if actual == scope
            ));
            assert!(app.shared_views.seeding.contains(&scope));
        }
        assert!(app.pane_move_destinations(pane.id).is_empty());
        assert!(!app.pane_has_move_destination(pane.id));
        drop(app.update(Message::MovePaneToChannel {
            card: pane.id,
            channel: destination,
        }));
        let generation = app.workspace_view_generation;
        drop(app.update(Message::PaneDropToChannel {
            generation,
            card: pane.id,
            channel: destination,
        }));
        assert!(has_notice(&app, SCOPE_MOVE_NOTICE));
        assert!(!app.pane_transfer_pending(pane.id));
        assert!(rx.try_recv().is_err());
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    }
}

#[test]
fn a_seed_ack_releases_the_source_move_gate_without_admitting_the_destination() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let scope = Scope::Channel(source);
    app.shared_views.initialized.remove(&scope);
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    app.persist();
    assert!(matches!(
        rx.try_recv().unwrap(),
        ModelEdit::SeedView { scope: actual, .. } if actual == scope
    ));
    assert!(!app.can_move_pane_to_channel(pane.id, destination));
    let model = app.workspace_model.clone();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
            scope,
            operation: None,
            model,
        })),
    );
    assert!(rx.try_recv().is_err());
    // A known authoritative destination need not have been opened/admitted in
    // this window. Only the source scope's initial seed owns the pending pane.
    app.shared_views
        .initialized
        .remove(&Scope::Channel(destination));
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    drop(app.update(Message::MovePaneToChannel {
        card: pane.id,
        channel: destination,
    }));
    assert!(matches!(
        rx.try_recv().unwrap(),
        ModelEdit::ChangeView {
            scope: actual,
            change: Change::Transfer { id, destination: target, .. },
            ..
        } if actual == Scope::Channel(source)
            && id == pane.id
            && target == Scope::Channel(destination)
    ));
    assert!(app.pane_transfer_pending(pane.id));
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_queued_transfer_owns_close_even_if_the_chat_now_awaits_admission() {
    for accepted in [false, true] {
        let (mut app, source, destination, pane) = retained_chat_move_fixture();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.update(Message::MovePaneToChannel {
            card: pane.id,
            channel: destination,
        }));
        let ModelEdit::ChangeView {
            operation,
            change: Change::Transfer { id, .. },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("the public Move must own the queued transfer");
        };
        assert_eq!(id, pane.id);
        assert!(app.pane_transfer_pending(pane.id));
        // Explicit saved-runtime fixture boundary after a real accepted Move.
        // Calling the startup initializer presents saved bytes as Awaiting;
        // this does not claim that a live provider changed state or re-ran
        // application startup between these native interactions.
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, DRAFT.into())]));
        drop(app.initialize_runtimes());
        assert!(app.chat_awaiting_admission(pane.id));
        assert!(!app.chat_start_pending(pane.id));
        let admission = app.durable.pending_chat_admissions[&pane.id].clone();
        let allocation = runtime_allocation(&app, pane.id);
        drop(app.update(Message::CloseCard(pane.id)));
        drop(app.update(Message::CommitClose(pane.id)));
        assert!(has_notice(&app, MOVE_CLOSE_NOTICE));
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert_eq!(runtime_allocation(&app, pane.id), allocation);
        assert_eq!(saved_text(&app, pane.id), DRAFT);
        assert_eq!(
            app.durable.pending_chat_admissions.get(&pane.id),
            Some(&admission)
        );
        assert!(app.pane_transfer_pending(pane.id));
        assert!(!app.chat_start_pending(pane.id));
        assert!(rx.try_recv().is_err(), "Close must not publish a Remove");

        if accepted {
            let mut model = app.workspace_model.clone();
            views::transfer(
                &mut model.views,
                &Scope::Channel(source),
                pane.id,
                &Scope::Channel(destination),
                None,
            )
            .unwrap();
            for view in &mut model.views {
                view.revision += 1;
            }
            model.sequence += 1;
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                    scope: Scope::Channel(source),
                    operation: Some(operation),
                    model,
                })),
            );
        } else {
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewRefused {
                    scope: Scope::Channel(source),
                    operation,
                    error: "fixture move was refused".into(),
                })),
            );
        }
        assert!(!app.pane_transfer_pending(pane.id));
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert_eq!(saved_text(&app, pane.id), DRAFT);
        assert!(rx.try_recv().is_err());
        let owner = if accepted { destination } else { source };
        assert_eq!(
            app.durable
                .channels
                .iter()
                .filter(|channel| channel.cards.contains(&pane.id))
                .map(|channel| channel.id)
                .collect::<Vec<_>>(),
            [owner]
        );
        // Canonical admission is still the existing path. This test never
        // polls its provider Task or authorizes a fresh start/cleanup itself.
    }
}
