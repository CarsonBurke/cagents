//! A terminal opened in a channel whose shared view is not admitted yet, as
//! on a device whose daemon has just started, waits for that view instead of
//! closing when the view first arrives without it (#4380).
use super::*;
use crate::app::workspace_model_sync::{EdgeChanges, ModelUpdate};

struct Window {
    app: Cagents,
    scope: Scope,
    rx: TestModelRequests,
}

fn window() -> Window {
    let (app, channel, _card, _target, rx) = unpublished_default_chat();
    Window {
        app,
        scope: Scope::Channel(channel),
        rx,
    }
}

fn drain(rx: &TestModelRequests) {
    while rx.try_recv().is_ok() {}
}

fn inserts(rx: &TestModelRequests, card: CardId) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|edit| {
            matches!(edit, ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)
        })
        .count()
}

fn new_terminal(window: &mut Window) -> CardId {
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(window.app.insert_terminal_card(session, None));
    let card = window.app.workspace.focused().unwrap();
    assert!(
        window
            .app
            .durable
            .pending_terminal_creations
            .contains(&card)
    );
    card
}

fn seed_applied(window: &mut Window, panes: Vec<Pane>) {
    let scope = window.scope.clone();
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let mut view = View::empty(scope.clone());
    view.revision = 1;
    view.panes = panes;
    model.views.retain(|view| view.scope != scope);
    model.views.push(view);
    let attempt = window.app.shared_views.seed_attempts[&scope];
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::SeedApplied {
                scope,
                attempt,
                model,
            }),
    );
}

fn assert_open(window: &Window, card: CardId) {
    assert!(
        window
            .app
            .card_state(card)
            .is_some_and(|card| card.existence.is_open()),
        "the new terminal closed before its view could accept it"
    );
    assert!(
        window
            .app
            .workspace
            .cards()
            .iter()
            .any(|shown| shown.id == card)
    );
}

#[test]
fn a_terminal_opened_while_its_channel_seeds_survives_the_seed() {
    let mut window = window();
    let scope = window.scope.clone();
    window.app.workspace_model.views.clear();
    window.app.shared_views.initialized.remove(&scope);
    window.app.shared_views.baseline.remove(&scope);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.seeding.contains(&scope));
    drain(&window.rx);

    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert_open(&window, card);
    // Nothing is inserted into a view still being seeded.
    assert_eq!(inserts(&window.rx, card), 0);

    seed_applied(&mut window, Vec::new());
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 1);
}

#[test]
fn a_terminal_opened_while_connecting_survives_joining_the_registry() {
    let mut window = window();
    window.app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
    window.app.workspace_model.views.clear();
    drop(window.app.reconcile_shared_views());
    drain(&window.rx);

    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert_open(&window, card);

    let mut model = window.app.workspace_model.clone();
    model.identity = uuid::Uuid::new_v4();
    model.sequence += 1;
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Connected(model)),
    );
    assert_open(&window, card);
    let mut early_inserts = 0;
    let seeded = std::iter::from_fn(|| window.rx.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::SeedView { scope, panes, .. } if scope == window.scope => Some(panes),
            ModelEdit::ChangeView {
                change: Change::Insert { pane, .. },
                ..
            } if pane.id == card => {
                early_inserts += 1;
                None
            }
            _ => None,
        })
        .expect("joining seeds the channel");
    assert_eq!(early_inserts, 0);
    // A creation still awaiting acceptance never migrates with the seed; it
    // is shared by its own Insert once the seed is in.
    assert!(seeded.iter().all(|pane| pane.id != card));
    seed_applied(&mut window, seeded);
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 1);
}

#[test]
fn a_terminal_opened_in_a_blocked_channel_survives_the_block() {
    let mut window = window();
    // A terminal here whose canonical pane is listed by a view this window
    // cannot project holds the channel back until that view goes. The
    // previous pass already found it held back.
    window.app.shared_views.blocked.insert(window.scope.clone());
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(window.app.insert_terminal_card(session, None));
    let alias = window.app.workspace.focused().unwrap();
    let mut canonical = Pane::from_card(window.app.card_state(alias).unwrap(), None).unwrap();
    canonical.id = CardId::new();
    let mut elsewhere = View::empty(ChannelId::new());
    elsewhere.panes.push(canonical);
    window.app.workspace_model.views.push(elsewhere);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.blocked.contains(&window.scope));
    drain(&window.rx);

    let card = new_terminal(&mut window);
    assert_ne!(card, alias);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.blocked.contains(&window.scope));
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 0);

    window
        .app
        .workspace_model
        .views
        .retain(|view| view.scope == window.scope);
    drop(window.app.reconcile_shared_views());
    assert!(!window.app.shared_views.blocked.contains(&window.scope));
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 1);
}

fn refuse(window: &mut Window, card: CardId, error: &str) {
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewRefused {
                scope: window.scope.clone(),
                operation: card.as_uuid(),
                error: error.into(),
            }),
    );
}

fn failure(window: &Window, card: CardId) -> Option<&str> {
    match window.app.cards.get(&card) {
        Some(CardRuntime::Failed(failure)) => Some(failure.message()),
        _ => None,
    }
}

#[test]
fn a_new_terminal_its_view_refuses_says_why_until_retried() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert_eq!(inserts(&window.rx, card), 1);

    // It never started, so its pane is the only place left to say so: it
    // stays, with the registry's reason, rather than closing without a word
    // (#4400).
    refuse(&mut window, card, "the channel is no longer open");
    assert_open(&window, card);
    assert_eq!(
        failure(&window, card),
        Some("The channel is no longer open.")
    );
    assert!(window.app.terminal_starts.lock().unwrap().is_empty());
    // No later view closes it or inserts it again on its own.
    for _ in 0..2 {
        drop(window.app.reconcile_shared_views());
    }
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 0);
    assert!(failure(&window, card).is_some());

    // Retry asks the view again, and the pane waits for its answer.
    drop(window.app.update(Message::RetryCard(card)));
    assert!(matches!(
        window.app.cards.get(&card),
        Some(CardRuntime::Loading)
    ));
    assert_eq!(inserts(&window.rx, card), 1);
    assert!(window.app.terminal_starts.lock().unwrap().is_empty());

    // Refused again, it says so again.
    refuse(&mut window, card, "the channel is no longer open");
    assert_open(&window, card);
    assert!(failure(&window, card).is_some());
}

#[test]
fn a_refused_new_terminal_closes_when_asked() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    refuse(&mut window, card, "refused");
    drain(&window.rx);

    drop(window.app.update(Message::CloseCard(card)));
    drop(window.app.update(Message::CommitClose(card)));
    for _ in 0..2 {
        drop(window.app.reconcile_shared_views());
    }
    assert!(
        !window
            .app
            .card_state(card)
            .is_some_and(|card| card.existence.is_open())
    );
    assert!(
        !window
            .app
            .durable
            .pending_terminal_creations
            .contains(&card)
    );
    assert!(
        !window
            .app
            .shared_views
            .refused_creations
            .contains_key(&card)
    );
    // Nothing was ever shared, so there is nothing to remove.
    assert!(
        std::iter::from_fn(|| window.rx.try_recv().ok()).all(|edit| !matches!(
            edit,
            ModelEdit::ChangeView { change: Change::Remove { id }, .. } if id == card
        ))
    );
}

#[test]
fn a_new_terminal_that_fails_to_start_says_it_did_not_open_and_why() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    admit_insert(&mut window, card);
    let CardKind::Terminal(session) = &window.app.card_state(card).unwrap().kind else {
        unreachable!()
    };
    let terminal = session.id;
    drop(window.app.sync_chrome());
    let pane = window.app.notice_pane_name(card).unwrap();
    let reason = "terminal worker failed to start: pin terminal launch directory /repo: No such file or directory (os error 2)";
    drop(window.app.update(Message::TerminalStarted {
        card,
        terminal,
        result: Err(reason.into()),
    }));

    // Nothing ran, so it was never something to restore or reconnect: the
    // pane says it didn't open, with the reason (#4400).
    assert_open(&window, card);
    assert!(window.app.terminal_creation_failed(card));
    assert_eq!(
        failure(&window, card),
        Some("The terminal’s folder is missing: /repo.")
    );
    let Some(CardRuntime::Failed(failure)) = window.app.cards.get(&card) else {
        panic!("expected the failed terminal pane");
    };
    assert_eq!(
        failure.creation_guidance(),
        Some(
            "Restore the folder and choose Retry, or close this pane and open a terminal in another folder."
        )
    );
    assert_eq!(
        terminal_failure::creation_title(None),
        "Couldn't open this terminal"
    );
    let notice = &window
        .app
        .notices
        .entries
        .iter()
        .find(|entry| entry.notice.key == Some(NoticeKey::TerminalStart(card)))
        .expect("notice about this terminal")
        .notice;
    assert_eq!(notice.message, format!("Couldn't open {pane}"));
    assert_eq!(notice.details.as_deref(), Some(reason));
}

/// A saved terminal restored without its process, which never recorded an
/// exit, is still one to restore when its start fails, not a new terminal
/// that didn't open.
#[test]
fn a_restored_terminal_that_fails_still_reads_as_one_to_restore() {
    let mut window = window();
    let card = new_terminal(&mut window);
    // Accepted long ago: no longer a creation, nor a pending start.
    Arc::make_mut(&mut window.app.durable)
        .pending_terminal_creations
        .remove(&card);
    let CardKind::Terminal(session) = &window.app.card_state(card).unwrap().kind else {
        unreachable!()
    };
    let terminal = session.id;
    assert!(!window.app.terminal_never_started(card));
    drop(window.app.update(Message::TerminalStarted {
        card,
        terminal,
        result: Err("the daemon closed the connection before responding".into()),
    }));
    assert!(!window.app.terminal_creation_failed(card));
    assert!(matches!(
        window.app.cards.get(&card),
        Some(CardRuntime::Failed(terminal_failure::Failure::Terminal(_)))
    ));
}

/// The registry accepts the window's Insert of `card`, holding its start.
fn admit_insert(window: &mut Window, card: CardId) {
    admit_insert_with(window, card, |_, _| {});
}

/// [`admit_insert`], with `also` changing the accepting model.
fn admit_insert_with(
    window: &mut Window,
    card: CardId,
    also: impl FnOnce(&mut crate::workspace_model::WorkspaceModel, &Pane),
) {
    let scope = window.scope.clone();
    let (operation, pane) = std::iter::from_fn(|| window.rx.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                change: Change::Insert { pane, .. },
                ..
            } if pane.id == card => Some((operation, pane)),
            _ => None,
        })
        .expect("the new terminal was published");
    assert!(pane.pending_terminal_start);
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let view = model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap();
    view.revision += 1;
    view.panes.push(pane.clone());
    also(&mut model, &pane);
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope,
                operation: Some(operation),
                model,
            }),
    );
}

/// A terminal whose start failed once its view held it has no source to
/// close, so its Close removes it by identity instead of being kept with
/// "couldn't be removed right now".
#[test]
fn an_admitted_new_terminal_that_failed_to_start_closes_when_asked() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    admit_insert(&mut window, card);
    assert!(
        window
            .app
            .workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .any(|pane| pane.id == card && pane.pending_terminal_start)
    );
    let CardKind::Terminal(session) = &window.app.card_state(card).unwrap().kind else {
        unreachable!()
    };
    let terminal = session.id;
    drop(window.app.update(Message::TerminalStarted {
        card,
        terminal,
        result: Err("terminal worker failed to start".into()),
    }));
    assert!(window.app.terminal_creation_failed(card));
    drain(&window.rx);

    drop(window.app.update(Message::CloseCard(card)));
    drop(window.app.update(Message::CommitClose(card)));
    drop(window.app.reconcile_shared_views());
    assert!(
        !window
            .app
            .card_state(card)
            .is_some_and(|card| card.existence.is_open()),
        "the failed terminal was kept"
    );
    assert!(
        !window
            .app
            .notices
            .entries
            .iter()
            .any(|entry| { entry.notice.message == crate::app::shared_views::PANE_KEPT })
    );
    let removals = std::iter::from_fn(|| window.rx.try_recv().ok())
        .filter(|edit| {
            matches!(edit, ModelEdit::ChangeView { change: Change::Remove { id }, .. } if *id == card)
        })
        .count();
    assert_eq!(removals, 1);
}

/// A peer can complete the start between this window's Close and its
/// commit. The pane then has a source, so nothing removes it by identity.
#[test]
fn a_failed_new_terminal_a_peer_started_is_not_removed_by_identity() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    admit_insert(&mut window, card);
    let CardKind::Terminal(session) = &window.app.card_state(card).unwrap().kind else {
        unreachable!()
    };
    let terminal = session.id;
    drop(window.app.update(Message::TerminalStarted {
        card,
        terminal,
        result: Err("terminal worker failed to start".into()),
    }));
    drain(&window.rx);

    drop(window.app.update(Message::CloseCard(card)));
    let scope = window.scope.clone();
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let view = model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap();
    view.revision += 1;
    view.panes
        .iter_mut()
        .find(|pane| pane.id == card)
        .unwrap()
        .pending_terminal_start = false;
    window.app.workspace_model = model;
    drop(window.app.update(Message::CommitClose(card)));
    drop(window.app.reconcile_shared_views());
    assert!(
        std::iter::from_fn(|| window.rx.try_recv().ok()).all(|edit| !matches!(
            edit,
            ModelEdit::ChangeView { change: Change::Remove { id }, .. } if id == card
        ))
    );
}

/// An Insert that never reached the publisher's queue was never sent: a view
/// without the terminal doesn't close it, and the next reconciliation sends
/// the Insert again (#4490).
#[test]
fn a_new_terminal_whose_insert_was_not_queued_is_sent_again() {
    let mut window = window();
    drain(&window.rx);
    // The publisher has stopped: nothing can be queued.
    let (closed, receiver) = crate::app::workspace_model_sync::test_model_channel();
    drop(receiver);
    let open = std::mem::replace(&mut window.app.workspace_model_tx, closed);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.holds_unqueued_creation());
    // Nothing waits for an answer to an edit that never went.
    assert!(
        window
            .app
            .shared_views
            .pending
            .iter()
            .all(|(_, _, change)| {
                !matches!(change, Change::Insert { pane, .. } if pane.id == card)
            })
    );

    // Another edit to the view, which doesn't list it.
    let scope = window.scope.clone();
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap()
        .revision += 1;
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope,
                operation: None,
                model,
            }),
    );
    assert_open(&window, card);

    window.app.workspace_model_tx = open;
    drop(window.app.reconcile_shared_views());
    assert_eq!(inserts(&window.rx, card), 1);
    assert!(!window.app.holds_unqueued_creation());
    assert_open(&window, card);
}

/// Once its Insert goes unqueued, a new terminal is owed a reconciliation by
/// every edge, as an unpublished conversation is, until the Insert is
/// queued; a refused one waits for its Retry instead (#4490).
#[test]
fn an_unqueued_new_terminal_is_owed_reconciliation() {
    let mut window = window();
    drain(&window.rx);
    let (closed, receiver) = crate::app::workspace_model_sync::test_model_channel();
    drop(receiver);
    let open = std::mem::replace(&mut window.app.workspace_model_tx, closed);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.holds_unqueued_creation());
    window.app.workspace_model_tx = open;
    window.app.shared_views.reconciled_roots = window.app.worktree_roots_digest();
    assert!(!window.app.holds_unpublished_local_chat());
    let identity = window.app.workspace_model.identity;
    let sequence = window.app.workspace_model.sequence;
    // This edge installs no new state. Only the reconciliation owed to the
    // terminal should retry publishing it.
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Changed {
                sequence,
                edit: ModelEdit::OpenProject {
                    repository: "/repo".into(),
                    name: "repo".into(),
                    kind: ProjectKind::Git,
                },
                changes: EdgeChanges {
                    identity: Some(identity),
                    ..EdgeChanges::default()
                },
            }),
    );
    assert_eq!(inserts(&window.rx, card), 1);
    assert!(!window.app.holds_unqueued_creation());
    refuse(&mut window, card, "refused");
    assert!(!window.app.holds_unqueued_creation());
}

/// Accepts `card`'s Insert into a view whose registry runs another session
/// for its agent than the pane saved, which fences the pane.
fn admit_fenced(window: &mut Window, card: CardId) {
    let scope = window.scope.clone();
    admit_insert_with(window, card, |model, pane| {
        let CardKind::Terminal(saved) = &pane.kind else {
            unreachable!()
        };
        // The registry's agent runs another session than the pane saved.
        let running = fresh_terminal_session(saved.launch.clone());
        model.agents.push(crate::agent_execution::Agent {
            id: crate::model::AgentId::from_uuid(card.as_uuid()),
            name: None,
            origin: crate::agent_execution::Origin::User,
            spec: crate::agent_execution::Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: crate::agent_execution::Target {
                    repository: saved.launch.working_directory.clone(),
                    directory: saved.launch.working_directory.clone(),
                    placement: Some(scope.clone()),
                },
                surface: crate::agent_execution::Surface::Terminal,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: scope.clone(),
            session: Some(crate::agent_execution::HostedSession::Terminal(
                running.clone(),
            )),
            console: Some(running),
            created_ms: 0,
            closed: false,
        });
    });
    assert!(window.app.shared_views.quarantined.contains(&card));
}

/// A new terminal its view accepts while fencing its agent waits for no
/// Insert: it shows as a fenced pane, and closes (#4490).
#[test]
fn a_new_terminal_its_view_fences_waits_for_no_insert_and_closes() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    admit_fenced(&mut window, card);
    assert_open(&window, card);
    assert!(!window.app.unpublished_pane(card));
    assert!(!window.app.holds_unqueued_creation());
    drain(&window.rx);
    drop(window.app.reconcile_shared_views());
    assert_eq!(inserts(&window.rx, card), 0);

    drop(window.app.update(Message::CloseCard(card)));
    drop(window.app.update(Message::CommitClose(card)));
    drop(window.app.reconcile_shared_views());
    assert!(
        !window
            .app
            .card_state(card)
            .is_some_and(|card| card.existence.is_open())
    );
}

/// Once its fence lifts, a new terminal its view accepted while fenced
/// starts, as one accepted unfenced does (#4490).
#[test]
fn a_new_terminal_starts_once_its_fence_lifts() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    admit_fenced(&mut window, card);
    assert!(
        window
            .app
            .durable
            .pending_terminal_creations
            .contains(&card)
    );

    // The registry no longer runs another session for it.
    let scope = window.scope.clone();
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    model
        .agents
        .retain(|agent| agent.id.as_uuid() != card.as_uuid());
    model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap()
        .revision += 1;
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope,
                operation: None,
                model,
            }),
    );
    assert!(!window.app.shared_views.quarantined.contains(&card));
    assert_open(&window, card);
    // Admitted, its creation is settled and its start under way.
    assert!(
        !window
            .app
            .durable
            .pending_terminal_creations
            .contains(&card)
    );
    assert!(window.app.shared_views.attached.contains(&card));
}
