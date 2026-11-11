use super::*;

pub(super) fn writer_fixture(app: &mut Cagents) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    install_writer(app, &directory);
    directory
}

fn install_writer(app: &mut Cagents, directory: &tempfile::TempDir) {
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("replica-capture"),
    )
    .unwrap();
    let settings_path = directory.path().join("settings.toml");
    settings::save(&settings_path, &app.settings).unwrap();
    app.settings = settings::load(&settings_path).unwrap();
    app.settings_base = app.settings.clone();
    app.settings_path = Some(settings_path);
    app.settings_write_blocked = false;
    app.session = Some(writer);
    app.save_workspace();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
}

/// A complete retained Claude descriptor, using the existing terminal proof
/// builder and the production retained-actor continuity/session-ID contract.
pub(super) fn retained_chat(title: &str) -> CardState {
    let CardKind::Terminal(terminal) = moving_terminal_kind() else {
        unreachable!()
    };
    let mut recipe = terminal.launch;
    recipe.program = crate::agent::Provider::Claude.executable().into();
    recipe.arguments.clear();
    let session = crate::chat::Session {
        id: TerminalId::new(),
        proof: terminal.continuity.live_proof(),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
        },
    };
    assert!(session.has_valid_native_continuity());
    CardState {
        id: CardId::new(),
        generation: 0,
        title: title.into(),
        width_fraction: 0.5,
        existence: Existence::Open,
        kind: CardKind::Chat(session),
    }
}

#[test]
fn presentation_replica_capture_updates_immediately_and_reopens_the_latest_sqlite_state() {
    let (mut app, source, _, pane) = presentation_fixture();
    // Match the saved-history shape and managed unread working set. The
    // initialized baselines below represent a settled connected window.
    let destination = app
        .durable
        .channels
        .iter()
        .find(|row| row.id != source)
        .unwrap()
        .id;
    let mut managed = Vec::new();
    for index in 1..1_769 {
        let mut saved = app.card_state(pane.id).unwrap().clone();
        saved.id = CardId::new();
        if let CardKind::Browser(browser) = &mut saved.kind {
            browser.id = BrowserId::new();
        }
        if index < 83 {
            let path = PathBuf::from(format!("/repo/capture-{index}"));
            app.workspace_model.views.push(View {
                scope: Scope::Worktree {
                    repository: PathBuf::from("/repo/.git"),
                    path: path.clone(),
                },
                revision: 1,
                panes: vec![Pane::from_card(&saved, Some(path.clone())).unwrap()],
                retired: vec![],
            });
            Arc::make_mut(&mut app.durable)
                .card_scopes
                .insert(saved.id, path);
            app.cards.insert(saved.id, CardRuntime::Completed);
            if index <= 20 {
                managed.push(saved.id);
            }
        } else {
            saved.existence = super::super::super::project_ui::removed_existence("saved history");
        }
        Arc::make_mut(&mut app.durable).cards.push(saved);
    }
    while app.workspace_model.views.len() < 1_856 {
        app.workspace_model.views.push(View::empty(Scope::Worktree {
            repository: PathBuf::from("/repo/.git"),
            path: PathBuf::from(format!(
                "/repo/capture-history-{}",
                app.workspace_model.views.len()
            )),
        }));
    }
    let notices = managed
        .iter()
        .map(|id| presentation_managed_notice(&mut app, destination, *id))
        .collect();
    drop(app.update_notifications(notification_ui::Action::Snapshot(
        crate::notifications::Snapshot {
            notices,
            diagnostic: None,
        },
    )));
    let attention = managed
        .iter()
        .map(|id| app.sidebar_state.card_attention(*id))
        .collect::<Vec<_>>();
    for view in &app.workspace_model.views {
        app.shared_views
            .initialize(view.scope.clone(), view.panes.clone());
    }
    drop(app.sync_chrome());
    let directory = writer_fixture(&mut app);
    let initial = app.durable.clone();
    let initial_cards = initial.cards.as_ptr();
    let captures = app.replica_capture.captures;
    let offers = app.replica_capture.offers;
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let runtime = app.cards.get(&pane.id).unwrap() as *const CardRuntime;
    let mut separated_cards = None;
    for revision in 21..25 {
        let title = format!("Replica {revision}");
        let update = presentation_edge(&app, Scope::Channel(source), revision, &title, 0.73);
        drop(app.update(Message::WorkspaceModelObserved(update)));
        assert_eq!(app.card_state(pane.id).unwrap().title, title);
        assert_eq!(app.workspace_model.views[0].revision, revision);
        assert_eq!(
            app.shared_views.baseline[&Scope::Channel(source)][0].title,
            title
        );
        assert_eq!(app.replica_capture.captures, captures);
        assert_eq!(app.replica_capture.offers, offers);
        assert!(app.replica_capture.pending.is_some());
        assert!(app.replica_capture.timer_armed);
        let cards = app.durable.cards.as_ptr();
        assert_ne!(
            cards, initial_cards,
            "first edit separates the deliberately held prior revision"
        );
        assert_eq!(
            *separated_cards.get_or_insert(cards),
            cards,
            "later replica edits do not deep-copy the saved card history"
        );
        assert_eq!(
            app.cards.get(&pane.id).unwrap() as *const CardRuntime,
            runtime
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "replicated edits are never republished"
    );
    assert_eq!(app.durable.cards.len(), 1_769);
    assert_eq!(app.workspace_model.views.len(), 1_856);
    assert_eq!(
        managed
            .iter()
            .map(|id| app.sidebar_state.card_attention(*id))
            .collect::<Vec<_>>(),
        attention
    );
    // Capturing or inspecting an Arc is not an accepted snapshot offer.
    drop(app.persistent_workspace());
    drop(app.shared_window_snapshot());
    assert!(app.replica_capture.pending.is_some());
    assert_eq!(app.replica_capture.captures, captures);
    let generation = app.replica_capture.generation;
    let due = app
        .replica_capture
        .pending
        .as_ref()
        .unwrap()
        .changes
        .deadline();
    drop(app.replica_capture_due(generation, due));
    assert!(app.replica_capture.pending.is_none());
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert_eq!(app.replica_capture.offers, offers + 1);
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let mut writer = app.session.take().unwrap();
    writer.shutdown();
    let (mut reopened, restored) = crate::session::SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("replica-capture"),
    )
    .unwrap();
    let restored = restored.unwrap();
    let saved = restored
        .cards
        .iter()
        .find(|card| card.id == pane.id)
        .unwrap();
    assert_eq!(saved.title, "Replica 24");
    assert_eq!(saved.width_fraction, 0.73);
    reopened.shutdown();
    drop(app);
}

#[test]
fn presentation_replica_capture_has_one_trailing_timer_and_a_continuous_cap() {
    let (mut app, _, _, _) = presentation_fixture();
    let _directory = writer_fixture(&mut app);
    let start = Instant::now();
    app.mark_replica_snapshot(start);
    let generation = app.replica_capture.generation;
    drop(app.arm_replica_capture());
    for elapsed in [100, 200, 400, 600, 900] {
        app.mark_replica_snapshot(start + Duration::from_millis(elapsed));
        assert_eq!(app.replica_capture.generation, generation);
        drop(app.arm_replica_capture());
        assert!(app.replica_capture.timer_armed);
    }
    assert_eq!(
        app.replica_capture
            .pending
            .as_ref()
            .unwrap()
            .changes
            .deadline(),
        start + Duration::from_secs(1)
    );
    let captures = app.replica_capture.captures;
    drop(app.replica_capture_due(generation, start + Duration::from_millis(250)));
    assert_eq!(app.replica_capture.captures, captures);
    assert!(app.replica_capture.timer_armed);
    drop(app.replica_capture_due(generation, start + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert!(app.replica_capture.pending.is_none());
    drop(app.replica_capture_due(generation, start + Duration::from_secs(3)));
    assert_eq!(
        app.replica_capture.captures,
        captures + 1,
        "late old timer is inert"
    );
    drop(app);
}

#[test]
fn presentation_replica_capture_bare_flush_materializes_before_its_receipt() {
    let (mut app, source, _, pane) = presentation_fixture();
    let _directory = writer_fixture(&mut app);
    let update = presentation_edge(&app, Scope::Channel(source), 21, "Before flush", 0.62);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    let generation = app.replica_capture.generation;
    assert!(app.replica_capture.pending.is_some());
    let save = app.capture_workspace_save().unwrap();
    assert!(app.replica_capture.pending.is_none());
    save.flush
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let offers = app.replica_capture.offers;
    drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.offers, offers);
    assert_eq!(app.card_state(pane.id).unwrap().title, "Before flush");
    drop(app);
}

#[test]
fn presentation_replica_capture_rejection_preserves_debt_without_timer_spin() {
    let (mut app, _, _, _) = presentation_fixture();
    let _directory = writer_fixture(&mut app);
    app.session.as_mut().unwrap().shutdown();
    let start = Instant::now();
    app.mark_replica_snapshot(start);
    drop(app.arm_replica_capture());
    let generation = app.replica_capture.generation;
    drop(app.replica_capture_due(generation, start + Duration::from_secs(1)));
    assert_eq!(
        app.replica_capture.pending.as_ref().unwrap().changes.first,
        start
    );
    assert!(app.replica_capture.rejected);
    assert!(!app.replica_capture.timer_armed);
    let captures = app.replica_capture.captures;
    app.mark_replica_snapshot(start + Duration::from_secs(2));
    drop(app.arm_replica_capture());
    assert!(!app.replica_capture.timer_armed);
    assert_eq!(app.replica_capture.captures, captures);
    drop(app);
}

#[test]
fn presentation_replica_capture_owner_change_and_navigation_fence_old_timers() {
    let (mut app, source, destination, _) = presentation_fixture();
    let directory = writer_fixture(&mut app);
    app.mark_replica_snapshot(Instant::now());
    drop(app.arm_replica_capture());
    let generation = app.replica_capture.generation;
    let captures = app.replica_capture.captures;
    let mut old = app.session.take().unwrap();
    let (new, _) = crate::session::SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("replica-capture"),
    )
    .unwrap();
    app.session = Some(new);
    drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.captures, captures);
    assert!(app.replica_capture.pending.is_some());
    assert!(app.replica_capture.rejected);
    old.shutdown();
    app.materialize_replica_snapshot();
    assert!(app.replica_capture.pending.is_none());
    let update = presentation_edge(&app, Scope::Channel(source), 21, "Navigate latest", 0.65);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    let generation = app.replica_capture.generation;
    assert!(app.replica_capture.pending.is_some());
    drop(app.select_channel(destination));
    assert!(app.replica_capture.pending.is_none());
    let offers = app.replica_capture.offers;
    drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.offers, offers);
    drop(app);
}

#[test]
fn presentation_replica_capture_drop_flushes_latest_without_a_timer_delivery() {
    let (mut app, source, _, pane) = presentation_fixture();
    let directory = writer_fixture(&mut app);
    let update = presentation_edge(&app, Scope::Channel(source), 21, "Saved by Drop", 0.66);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert!(app.replica_capture.pending.is_some());
    drop(app);
    let (mut writer, restored) = crate::session::SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("replica-capture"),
    )
    .unwrap();
    let restored = restored.unwrap();
    let saved = restored
        .cards
        .iter()
        .find(|card| card.id == pane.id)
        .unwrap();
    assert_eq!(
        (saved.title.as_str(), saved.width_fraction),
        ("Saved by Drop", 0.66)
    );
    writer.shutdown();
}

#[test]
fn presentation_replica_capture_identity_fence_rejects_a_timer_without_discarding_debt() {
    for change_workspace in [false, true] {
        let (mut app, _, _, _) = presentation_fixture();
        let _directory = writer_fixture(&mut app);
        app.mark_replica_snapshot(Instant::now());
        drop(app.arm_replica_capture());
        let generation = app.replica_capture.generation;
        let first = app.replica_capture.pending.as_ref().unwrap().changes.first;
        let old_id = app.durable.id;
        let registry = app.workspace_model.identity;
        if change_workspace {
            Arc::make_mut(&mut app.durable).id = crate::model::WorkspaceId::new();
        } else {
            app.workspace_model.identity = uuid::Uuid::new_v4();
        }
        let captures = app.replica_capture.captures;
        drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
        assert_eq!(app.replica_capture.captures, captures);
        assert_eq!(
            app.replica_capture.pending.as_ref().unwrap().changes.first,
            first
        );
        assert!(app.replica_capture.rejected);
        Arc::make_mut(&mut app.durable).id = old_id;
        app.workspace_model.identity = registry;
        app.save_workspace();
        assert!(app.replica_capture.pending.is_none());
        drop(app);
    }
}

#[test]
fn presentation_replica_capture_normal_save_absorbs_once_and_keeps_reported_title_hold() {
    let (mut app, source, _, pane) = presentation_fixture();
    let _directory = writer_fixture(&mut app);
    let update = presentation_edge(&app, Scope::Channel(source), 21, "Replicated", 0.68);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    let generation = app.replica_capture.generation;
    app.reported_title_save.cards.insert(pane.id);
    app.card_state_mut(pane.id).unwrap().title = "Held local reported title".into();
    let captures = app.replica_capture.captures;
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    app.save_workspace();
    assert!(app.reported_title_save.cards.contains(&pane.id));
    assert!(app.replica_capture.pending.is_none());
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert!(
        rx.try_recv().is_err(),
        "absorbing local snapshot debt does not release a reported title"
    );
    drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.captures, captures + 1);
    drop(app);
}

#[test]
fn presentation_replica_capture_keeps_exact_acknowledgments_in_both_echo_orders() {
    use super::super::super::workspace_model_sync::ModelUpdate;
    use crate::workspace_model::delta::EditReceipt;
    for observer_first in [false, true] {
        let (mut app, source, _, pane) = presentation_fixture();
        let _directory = writer_fixture(&mut app);
        let observer = presentation_edge(&app, Scope::Channel(source), 27, "Own accepted", 0.69);
        let ModelUpdate::Changed {
            sequence,
            edit,
            changes,
        } = &observer
        else {
            unreachable!()
        };
        let ModelEdit::ChangeView {
            scope,
            operation,
            change,
            ..
        } = edit
        else {
            unreachable!()
        };
        app.shared_views
            .pending
            .push((*operation, scope.clone(), change.clone()));
        let operation = *operation;
        let receipt = ModelUpdate::Committed {
            receipt: EditReceipt {
                identity: app.workspace_model.identity,
                sequence: *sequence,
                effects: changes.effects.clone(),
            },
            edit: edit.clone(),
            views: changes.views.clone(),
        };
        app.card_state_mut(pane.id).unwrap().title = "Own accepted".into();
        app.card_state_mut(pane.id).unwrap().width_fraction = 0.69;
        let _ = app
            .workspace
            .set_card_width(pane.id, CardWidth::Fraction(0.69));
        let captures = app.replica_capture.captures;
        let (first, second) = if observer_first {
            (observer, receipt)
        } else {
            (receipt, observer)
        };
        drop(app.update(Message::WorkspaceModelObserved(first)));
        assert_eq!(
            app.shared_views
                .pending
                .iter()
                .any(|(id, _, _)| *id == operation),
            observer_first
        );
        drop(app.update(Message::WorkspaceModelObserved(second)));
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, _, _)| *id == operation)
        );
        assert_eq!(app.replica_capture.captures, captures);
        assert!(app.replica_capture.pending.is_some());
        app.materialize_replica_snapshot();
        assert_eq!(app.replica_capture.captures, captures + 1);
        drop(app);
    }
}

#[test]
fn presentation_replica_capture_poisoned_queue_does_not_acknowledge_a_snapshot() {
    let (mut app, _, _, _) = presentation_fixture();
    let _directory = writer_fixture(&mut app);
    app.mark_replica_snapshot(Instant::now());
    let first = app.replica_capture.pending.as_ref().unwrap().changes.first;
    app.session.as_ref().unwrap().poison_queue_for_test();
    app.materialize_replica_snapshot();
    assert_eq!(
        app.replica_capture.pending.as_ref().unwrap().changes.first,
        first
    );
    assert!(app.replica_capture.rejected);
    drop(app.arm_replica_capture());
    assert!(!app.replica_capture.timer_armed);
    drop(app);
}

#[test]
fn presentation_replica_capture_actual_registry_join_drains_and_invalidates() {
    use super::super::super::workspace_model_sync::ModelUpdate;
    let (mut app, source, _, _) = presentation_fixture();
    let _directory = writer_fixture(&mut app);
    let update = presentation_edge(&app, Scope::Channel(source), 21, "Before replacement", 0.64);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    let generation = app.replica_capture.generation;
    assert!(app.replica_capture.pending.is_some());
    let mut replacement = app.workspace_model.clone();
    replacement.identity = uuid::Uuid::new_v4();
    let identity = replacement.identity;
    drop(app.update(Message::WorkspaceModelObserved(
        ModelUpdate::ConnectedClosed {
            model: replacement,
            closed: vec![],
        },
    )));
    assert_eq!(app.durable.workspace_model_identity, Some(identity));
    assert!(app.replica_capture.pending.is_none());
    let offers = app.replica_capture.offers;
    drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.offers, offers);
    drop(app);
}

#[test]
fn presentation_replica_capture_ordered_ui_handoff_absorbs_the_replica_once() {
    let (mut app, source, _, mut pane) = retained_chat_move_fixture();
    pane.kind = retained_chat("Retained source").kind;
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.workspace_model
        .views
        .iter_mut()
        .flat_map(|view| &mut view.panes)
        .find(|row| row.id == pane.id)
        .unwrap()
        .kind = pane.kind.clone();
    app.shared_views
        .initialize(Scope::Channel(source), vec![pane.clone()]);
    let directory = writer_fixture(&mut app);
    app.mark_replica_snapshot(Instant::now());
    drop(app.arm_replica_capture());
    let generation = app.replica_capture.generation;
    let offers = app.replica_capture.offers;
    let CardKind::Chat(source_session) = &pane.kind else {
        unreachable!()
    };
    let recipe = source_session
        .launch
        .terminal_recipe()
        .expect("confirmed Claude resume recipe");
    let CardKind::Terminal(mut terminal) = moving_terminal_kind() else {
        unreachable!()
    };
    terminal.launch = recipe;
    assert_ne!(terminal.id, source_session.id);
    assert!(source_session.launch.resumes_in_terminal(&terminal.launch));
    drop(app.finish_chat_terminal_handoff(pane.id, terminal.clone()));
    assert!(app.replica_capture.pending.is_none());
    assert_eq!(app.replica_capture.offers, offers + 1);
    assert_eq!(
        app.card_state(pane.id).unwrap().kind,
        CardKind::Terminal(terminal.clone())
    );
    drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.offers, offers + 1);
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let mut writer = app.session.take().unwrap();
    writer.shutdown();
    let (mut reopened, restored) = crate::session::SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("replica-capture"),
    )
    .unwrap();
    let restored = restored.unwrap();
    // Proofless restore under the fixture's different boot keeps the exact
    // resume descriptor while downgrading its live continuity.
    let expected = TerminalSession {
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: terminal.launch.clone(),
            last_exit: None,
        },
        ..terminal
    };
    assert_eq!(
        restored
            .cards
            .iter()
            .find(|card| card.id == pane.id)
            .unwrap()
            .kind,
        CardKind::Terminal(expected)
    );
    reopened.shutdown();
    drop(app);
}

#[test]
fn presentation_replica_capture_memory_only_navigation_does_not_acknowledge_replica_debt() {
    let (mut app, _, _, _) = presentation_fixture();
    assert!(app.session.is_none());
    drop(app.save_navigation_soon());
    assert!(app.navigation_save_pending);
    let offers = app.replica_capture.offers;
    app.save_workspace();
    assert!(!app.navigation_save_pending);
    assert_eq!(app.replica_capture.offers, offers);
    let _directory = writer_fixture(&mut app);
    app.mark_replica_snapshot(Instant::now());
    let first = app.replica_capture.pending.as_ref().unwrap().changes.first;
    let mut writer = app.session.take().unwrap();
    writer.shutdown();
    drop(app.save_navigation_soon());
    app.save_workspace();
    assert!(
        app.navigation_save_pending,
        "debt with a missing writer is still owed"
    );
    assert_eq!(
        app.replica_capture.pending.as_ref().unwrap().changes.first,
        first
    );
    drop(app);
}

#[test]
fn presentation_replica_capture_trace_smoke_matches_deferred_work_accounting_tags() {
    // Declare the directory first: it also outlives app on assertion unwind.
    let directory = tempfile::tempdir().unwrap();
    let (mut app, source, _, _) = presentation_fixture();
    install_writer(&mut app, &directory);
    let log_path = directory.path().join("capture.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || log.try_clone().unwrap())
        .finish();
    crate::test_tracing::with_thread_subscriber(subscriber, || {
        let update = presentation_edge(&app, Scope::Channel(source), 21, "Trace replica", 0.67);
        drop(app.update(Message::WorkspaceModelObserved(update)));
        let generation = app.replica_capture.generation;
        // Drive an overdue offer and then its stale delivery without any
        // sub-second scheduling assumption; early rearming has clock tests.
        app.replica_capture.pending.as_mut().unwrap().changes =
            crate::session::SnapshotChanges::at(Instant::now() - Duration::from_secs(2));
        drop(app.update(Message::ReplicaCaptureDue(generation)));
        drop(app.update(Message::ReplicaCaptureDue(generation)));
    });
    drop(app);
    let logged = std::fs::read_to_string(log_path).unwrap();
    let tag = |line: &str, field: &str, value: &str| {
        let quoted = format!("{field}=\"{value}\"");
        let plain = format!("{field}={value}");
        line.split_whitespace()
            .any(|token| token == quoted || token == plain)
    };
    let rows = logged.lines().collect::<Vec<_>>();
    let staged = |stage: &str| {
        rows.iter()
            .copied()
            .filter(|line| tag(line, "stage", stage))
            .collect::<Vec<_>>()
    };
    assert_eq!(staged("ui_update").len(), 1, "{logged}");
    let timers = staged("replica_snapshot_timer");
    assert_eq!(timers.len(), 2, "{logged}");
    assert!(timers.iter().all(|line| line.contains("elapsed_ns=")));
    let wakes = staged("replica_snapshot_wake");
    assert_eq!(wakes.len(), timers.len(), "{logged}");
    for outcome in ["capture", "stale"] {
        assert_eq!(
            wakes
                .iter()
                .filter(|line| tag(line, "outcome", outcome))
                .count(),
            1,
            "{logged}"
        );
    }
    let captures = staged("window_snapshot_capture");
    assert_eq!(captures.len(), 1, "{logged}");
    assert!(tag(captures[0], "parent", "replica_timer"), "{logged}");
    assert!(tag(captures[0], "had_replica_debt", "true"), "{logged}");
    assert!(tag(captures[0], "accepted", "true"), "{logged}");
    assert!(captures[0].contains("elapsed_ns="));
}

#[test]
fn presentation_replica_capture_repository_load_is_a_structural_barrier() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, source, _, pane) = presentation_fixture();
    install_writer(&mut app, &directory);
    let update = presentation_edge(
        &app,
        Scope::Channel(source),
        21,
        "Before repository load",
        0.68,
    );
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert!(app.replica_capture.pending.is_some());
    let generation = app.replica_capture.generation;
    let captures = app.replica_capture.captures;
    let offers = app.replica_capture.offers;
    let project = ProjectId::new();
    let repository = PathBuf::from("/loaded/.git");
    app.workspace_model
        .projects
        .push(crate::workspace_model::ModelProject {
            repository: repository.clone(),
            name: "loaded".into(),
            kind: ProjectKind::Git,
            ordinal: 1,
        });
    app.repository_generations.insert(project, 1);
    let loaded = LoadedRepository {
        project: ProjectState {
            kind: ProjectKind::Git,
            id: project,
            generation: 0,
            name: "loaded".into(),
            repository,
            existence: Existence::Open,
        },
        repository: None,
        worktrees: vec![],
        github: GitHubAvailability::NotAssociated,
        review_repository: None,
        head_repositories: HashMap::new(),
    };
    drop(app.update(Message::RepositoryLoaded {
        open_cards: false,
        project_id: project,
        generation: 1,
        result: Ok(Box::new(loaded)),
    }));
    assert!(app.replica_capture.pending.is_none());
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert_eq!(app.replica_capture.offers, offers + 1);
    assert_eq!(
        app.card_state(pane.id).unwrap().title,
        "Before repository load"
    );
    assert_eq!(
        app.shared_views.baseline[&Scope::Channel(source)][0].title,
        "Before repository load"
    );
    drop(app.replica_capture_due(generation, Instant::now() + Duration::from_secs(2)));
    assert_eq!(app.replica_capture.captures, captures + 1);

    // Structural settlement must not disable the separate stable-presentation
    // path or make the following replica update take a full local projection.
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    app.shared_views.scope_discovery_count.set(0);
    let update = presentation_edge(
        &app,
        Scope::Channel(source),
        22,
        "After repository load",
        0.72,
    );
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.local_projection_count.get(), 0);
    assert_eq!(app.shared_views.selected_projection_count.get(), 1);
    assert_eq!(app.shared_views.scope_discovery_count.get(), 0);
    assert!(app.replica_capture.pending.is_some());
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert_eq!(
        app.card_state(pane.id).unwrap().title,
        "After repository load"
    );
    drop(app);
}

#[test]
fn presentation_replica_capture_repository_alias_adoption_keeps_draft_and_ordered_save() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, source, destination, pane) = presentation_fixture();
    let destination_project = ProjectId::new();
    let repository = PathBuf::from("/other/.git");
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == destination)
        .unwrap()
        .project_id = destination_project;
    app.sidebar_model.projects.push(SidebarProject::new(
        destination_project,
        RepositoryId::new(),
        "other".into(),
        repository.clone(),
        vec![],
        vec![],
    ));
    app.workspace_model
        .channels
        .iter_mut()
        .find(|row| row.id == destination)
        .unwrap()
        .repository = repository.clone();
    let alias = CardId::new();
    let state = CardState {
        id: alias,
        generation: 0,
        title: "Local terminal alias".into(),
        width_fraction: 0.5,
        existence: Existence::Open,
        kind: moving_terminal_kind(),
    };
    let alias_scope = ChannelId::new();
    let alias_pane = Pane::from_card(&state, None).unwrap();
    let mut local = app
        .durable
        .channels
        .iter()
        .find(|row| row.id == source)
        .unwrap()
        .clone();
    local.id = alias_scope;
    local.title = "Alias source".into();
    local.ordinal = 2;
    local.cards = vec![alias];
    local.focused = Some(alias);
    let mut shared = app.workspace_model.channel(source).unwrap().clone();
    shared.id = alias_scope;
    shared.title = local.title.clone();
    shared.ordinal = local.ordinal;
    Arc::make_mut(&mut app.durable).channels.push(local);
    Arc::make_mut(&mut app.durable).cards.push(state);
    app.workspace_model.channels.push(shared);
    app.workspace_model.views.push(View {
        scope: Scope::Channel(alias_scope),
        revision: 17,
        panes: vec![alias_pane.clone()],
        retired: vec![],
    });
    app.shared_views
        .initialize(Scope::Channel(alias_scope), vec![alias_pane.clone()]);
    app.cards.insert(alias, CardRuntime::Completed);
    app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(alias, "cross-repository draft  ".into())]));
    install_writer(&mut app, &directory);
    let update = presentation_edge(&app, Scope::Channel(source), 21, "Pending replica", 0.68);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert!(app.replica_capture.pending.is_some());

    // The incoming canonical identity moved across repositories while the
    // other channel still owes its already-published local snapshot.
    let mut canonical = alias_pane;
    canonical.id = CardId::new();
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(alias_scope))
        .unwrap()
        .panes
        .clear();
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap()
        .panes = vec![canonical.clone()];
    let captures = app.replica_capture.captures;
    let offers = app.replica_capture.offers;
    drop(app.reconcile_shared_views_for_repository(&repository, &HashSet::new()));
    assert!(app.replica_capture.pending.is_none());
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert_eq!(app.replica_capture.offers, offers + 1);
    assert!(!app.card_state(alias).unwrap().existence.is_open());
    let source_channel = app
        .durable
        .channels
        .iter()
        .find(|row| row.id == alias_scope)
        .unwrap();
    assert!(source_channel.cards.is_empty());
    assert_eq!(source_channel.focused, None);
    assert!(app.card_state(canonical.id).unwrap().existence.is_open());
    assert_eq!(app.card_state(pane.id).unwrap().title, "Pending replica");
    assert!(
        app.shared_views.baseline[&Scope::Channel(alias_scope)]
            .iter()
            .all(|pane| pane.id != alias)
    );
    assert!(
        app.shared_views.baseline[&Scope::Channel(destination)]
            .iter()
            .any(|pane| pane.id == canonical.id)
    );
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&canonical.id)
            .map(String::as_str),
        Some("cross-repository draft  ")
    );
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    drop(app);
    let (mut writer, restored) = crate::session::SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("replica-capture"),
    )
    .unwrap();
    let restored = restored.unwrap();
    let source_channel = restored
        .channels
        .iter()
        .find(|row| row.id == alias_scope)
        .unwrap();
    assert!(source_channel.cards.is_empty());
    assert_eq!(source_channel.focused, None);
    assert_eq!(
        restored.chat_drafts.get(&canonical.id).map(String::as_str),
        Some("cross-repository draft  ")
    );
    writer.shutdown();
}

#[test]
fn presentation_replica_capture_worktree_move_preserves_valid_source_focus() {
    for active_source in [false, true] {
        for keep_other_cards in [false, true] {
            for focus_moved in [false, true]
                .into_iter()
                .filter(|focus_moved| keep_other_cards || *focus_moved)
            {
                let directory = tempfile::tempdir().unwrap();
                let (mut app, source, other, pane) = presentation_fixture();
                let mut remaining = Vec::new();
                if keep_other_cards {
                    for title in ["First remaining", "Preserved focus"] {
                        let mut sibling = app.card_state(pane.id).unwrap().clone();
                        sibling.id = CardId::new();
                        sibling.title = title.into();
                        if let CardKind::Browser(browser) = &mut sibling.kind {
                            browser.id = BrowserId::new();
                        }
                        let sibling_pane = Pane::from_card(&sibling, None).unwrap();
                        remaining.push(sibling.id);
                        Arc::make_mut(&mut app.durable).cards.push(sibling);
                        Arc::make_mut(&mut app.durable)
                            .channels
                            .iter_mut()
                            .find(|row| row.id == source)
                            .unwrap()
                            .cards
                            .push(sibling_pane.id);
                        app.workspace_model
                            .views
                            .iter_mut()
                            .find(|view| view.scope == Scope::Channel(source))
                            .unwrap()
                            .panes
                            .push(sibling_pane.clone());
                        app.workspace
                            .add_card(
                                WorkspaceCard::new(
                                    sibling_pane.id,
                                    sibling_pane.title.clone(),
                                    CardWidth::Fraction(sibling_pane.width_fraction),
                                )
                                .unwrap(),
                                app.workspace.cards().len(),
                                false,
                            )
                            .unwrap();
                        app.shared_views
                            .baseline
                            .get_mut(&Scope::Channel(source))
                            .unwrap()
                            .push(sibling_pane);
                        app.cards
                            .insert(*remaining.last().unwrap(), CardRuntime::Completed);
                    }
                    Arc::make_mut(&mut app.durable)
                        .channels
                        .iter_mut()
                        .find(|row| row.id == source)
                        .unwrap()
                        .focused = if !focus_moved {
                        remaining.last().copied()
                    } else {
                        Some(pane.id)
                    };
                    if !focus_moved {
                        let _ = app.workspace.focus(*remaining.last().unwrap()).unwrap();
                    }
                }
                let expected_focus = if focus_moved {
                    remaining.first().copied()
                } else {
                    remaining.last().copied()
                };
                // Exercise a real source strip, rather than letting the active
                // snapshot replace durable membership with an older fixture strip.
                drop(app.select_channel(other));
                if active_source {
                    drop(app.select_channel(source));
                }
                install_writer(&mut app, &directory);
                let path = PathBuf::from("/repo/moved-worktree");
                let scope = Scope::Worktree {
                    repository: PathBuf::from("/repo/.git"),
                    path: path.clone(),
                };
                app.known_worktree_roots.insert(path.clone());
                let mut moved = pane.clone();
                moved.scope = Some(path.clone());
                app.workspace_model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(source))
                    .unwrap()
                    .panes
                    .retain(|row| row.id != pane.id);
                // Visit the destination first so its membership removal cannot
                // rely on an earlier source projection repairing focus for it.
                app.workspace_model.views.insert(
                    0,
                    View {
                        scope: scope.clone(),
                        revision: 22,
                        panes: vec![moved],
                        retired: vec![],
                    },
                );
                app.shared_views.initialize(scope.clone(), vec![]);
                drop(app.reconcile_shared_views_for_repository(
                    Path::new("/repo/.git"),
                    &HashSet::new(),
                ));
                let source_channel = app
                    .durable
                    .channels
                    .iter()
                    .find(|row| row.id == source)
                    .unwrap();
                assert_eq!(source_channel.cards, remaining);
                assert_eq!(source_channel.focused, expected_focus);
                assert_eq!(app.durable.card_scopes.get(&pane.id), Some(&path));
                assert!(app.card_state(pane.id).unwrap().existence.is_open());
                assert!(
                    app.workspace.card(pane.id).is_none(),
                    "the active snapshot must not put the moved pane back in its source channel"
                );
                // A second ordinary snapshot must retain the repaired membership
                // and focus instead of reintroducing an active strip's old state.
                app.save_workspace();
                let source_channel = app
                    .durable
                    .channels
                    .iter()
                    .find(|row| row.id == source)
                    .unwrap();
                assert_eq!(source_channel.cards, remaining);
                assert_eq!(source_channel.focused, expected_focus);
                drop(app.select_channel(source));
                assert!(
                    app.workspace.card(pane.id).is_none(),
                    "selecting the source must not resurrect the moved pane"
                );
                let source_channel = app
                    .durable
                    .channels
                    .iter()
                    .find(|row| row.id == source)
                    .unwrap();
                assert_eq!(source_channel.cards, remaining);
                assert_eq!(source_channel.focused, expected_focus);
                app.session
                    .as_ref()
                    .unwrap()
                    .flush(Duration::from_secs(5))
                    .unwrap();
                drop(app);
                let (mut writer, restored) = crate::session::SessionRuntime::open(
                    directory.path().join("window.sqlite3"),
                    crate::model::BootId::new("replica-capture"),
                )
                .unwrap();
                let restored = restored.unwrap();
                let source_channel = restored
                    .channels
                    .iter()
                    .find(|row| row.id == source)
                    .unwrap();
                assert_eq!(source_channel.cards, remaining);
                assert_eq!(source_channel.focused, expected_focus);
                assert_eq!(restored.card_scopes.get(&pane.id), Some(&path));
                writer.shutdown();
            }
        }
    }
}
