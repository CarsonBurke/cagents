mod history_start;
mod pane_follow;
mod published_card_edits;
mod removal_tests;
#[cfg(not(feature = "integrated-browser"))]
mod replica_capture;
mod repository_loads;
mod seed_refusal;
mod startup_admission;
mod startup_terminals;
mod unadmitted_first_send;
mod window_selection;
mod window_startup;

use super::*;
use crate::model::{
    BrowserId, BrowserRestoreFidelity, BrowserSession, ChannelId, ProjectChannel, ProjectId,
};
/// A report that a pane's conversation could not be reopened goes with
/// the pane, whichever path removes it; here, another client closing it.
#[test]
fn a_pane_closed_elsewhere_takes_its_reopen_failure_with_it() {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let report = |card: CardId| {
        TransientNotice::error("Could not reopen a conversation.")
            .with_details(format!("{card:?}"))
            .with_key(NoticeKey::ChatAttach(card))
    };
    app.notices.record(report(card));
    app.notices.record(report(CardId::new()));
    // Any message will do: every update ends by checking.
    let _ = app.update(Message::DismissNotice(u64::MAX));
    assert_eq!(
        app.notices
            .recent()
            .map(|entry| entry.notice.key.clone())
            .collect::<Vec<_>>(),
        [Some(NoticeKey::ChatAttach(card))],
        "an open pane keeps its report; one that is gone has none"
    );

    app.detach_shared_pane(card);
    let _ = app.update(Message::DismissNotice(u64::MAX));
    assert!(app.notices.is_empty());
}

fn fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
    let mut app = super::super::tests::context_inspector_test_app(false);
    let a = ChannelId::new();
    let b = ChannelId::new();
    let project = ProjectId::new();
    let pane = Pane {
        id: CardId::new(),
        title: "shared".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    };
    let durable = Arc::make_mut(&mut app.durable);
    for id in [a, b] {
        durable.channels.push(ProjectChannel {
            id,
            project_id: project,
            title: "view".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![],
            focused: None,
            checkout: None,
        });
    }
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.projects = vec![crate::workspace_model::ModelProject {
        repository: "/repo/.git".into(),
        name: "Fixture".into(),
        kind: Default::default(),
        ordinal: 1,
    }];
    app.workspace_model.channels = [a, b]
        .into_iter()
        .map(|id| crate::workspace_model::ModelChannel {
            id,
            repository: "/repo/.git".into(),
            title: "view".into(),
            title_source: Default::default(),
            ordinal: 1,
            checkout: None,
        })
        .collect();
    app.shared_views
        .initialized
        .extend([Scope::Channel(a), Scope::Channel(b)]);
    (app, a, b, pane)
}

#[test]
fn immutable_authority_lookup_borrows_the_first_project_repository() {
    let (mut app, _, _, _) = fixture();
    let shared = PathBuf::from("/shared-worktree");
    for repository in ["/first/.git", "/second/.git"] {
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            repository.into(),
            vec![crate::sidebar::SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                shared.clone(),
                None,
            )],
            vec![],
        ));
    }
    let borrowed = app.borrowed_view_scope_topology();
    let root_paths = app.worktree_root_paths();
    let owned = app.view_scope_topology(worktree_ui::WorktreeRoots::new(
        root_paths.iter().map(PathBuf::as_path),
    ));
    let first = app.sidebar_model.projects[0].repository.as_path();
    assert!(
        std::ptr::eq(borrowed.authorities.repository_for(&shared).unwrap(), first,),
        "a readiness lookup must not allocate another repository path value"
    );
    for path in [
        shared.as_path(),
        Path::new("/first"),
        Path::new("/first/.git"),
        Path::new("/second"),
    ] {
        assert_eq!(
            borrowed.authorities.repository_for(path),
            owned.authorities.repository_for(path)
        );
    }
    assert_eq!(owned.authorities.repository_for(&shared), Some(first));
    for repository in [Path::new("/first/.git"), Path::new("/second/.git")] {
        assert!(borrowed.authorities.contains_repository(repository));
        assert!(owned.authorities.contains_repository(repository));
    }
}

#[test]
fn alias_index_preserves_order_and_resource_kind_while_excluding_owned_and_closed_cards() {
    let (_, _, _, browser) = fixture();
    let (_, _, _, chat) = retained_chat_move_fixture();
    let terminal = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
    let shared_id = terminal.id;
    let mut kinds = vec![
        browser.kind,
        CardKind::Terminal(terminal),
        chat.kind,
        CardKind::Settings,
    ];
    if let CardKind::Browser(browser) = &mut kinds[0] {
        browser.id = BrowserId::from_uuid(shared_id.as_uuid());
    }
    if let CardKind::Chat(chat) = &mut kinds[2] {
        chat.id = shared_id;
    }
    let mut cards = Vec::new();
    let mut owned = HashSet::new();
    let mut expected = Vec::new();
    for kind in &kinds {
        let mut matching = Vec::new();
        for index in 0..4 {
            let id = CardId::new();
            cards.push(CardState {
                id,
                generation: 0,
                title: "alias".into(),
                width_fraction: 0.5,
                existence: if index == 2 {
                    super::super::project_ui::removed_existence("closed alias")
                } else {
                    Existence::Open
                },
                kind: kind.clone(),
            });
            if index == 3 {
                owned.insert(id);
            } else if index < 2 && !matches!(kind, CardKind::Settings) {
                matching.push(id);
            }
        }
        expected.push(matching);
    }
    let aliases = unowned_aliases(cards.iter(), &owned);
    for (kind, expected) in kinds.iter().zip(expected) {
        let actual = projection_resource(kind)
            .and_then(|resource| aliases.get(&resource))
            .into_iter()
            .flatten()
            .map(|card| card.id)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
}

fn channel_move_fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
    channel_move_fixture_with_kind(None)
}

fn channel_move_fixture_with_kind(kind: Option<CardKind>) -> (Cagents, ChannelId, ChannelId, Pane) {
    let (mut app, source, destination, mut pane) = fixture();
    if let Some(kind) = kind {
        pane.kind = kind;
    }
    app.workspace_model.channels = [source, destination]
        .into_iter()
        .map(|id| crate::workspace_model::ModelChannel {
            id,
            repository: PathBuf::from("/repo/.git"),
            title: "Channel".into(),
            title_source: Default::default(),
            ordinal: 1,
            checkout: None,
        })
        .collect();
    let mut source_view = View::empty(source);
    source_view.revision = 17;
    source_view.panes.push(pane.clone());
    app.workspace_model.views = vec![source_view, View::empty(destination)];
    Arc::make_mut(&mut app.durable).active_channel = Some(source);
    drop(app.reconcile_shared_views());
    if let CardKind::Terminal(session) = &pane.kind {
        // Projection dispatches an attach before returning its Task. This
        // fixture intentionally never polls that worker, so deliver its
        // explicit failed completion through the actual reducer before
        // testing a settled pane. Do not bypass the production start fence
        // or pretend that an actual terminal attachment succeeded.
        assert!(app.terminal_start_in_flight(pane.id));
        drop(app.update(Message::TerminalStarted {
            card: pane.id,
            terminal: session.id,
            result: Err("fixture terminal attach was intentionally not executed".into()),
        }));
        assert!(!app.terminal_start_in_flight(pane.id));
        assert!(matches!(
            app.cards.get(&pane.id),
            Some(CardRuntime::Failed(_))
        ));
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    }
    (app, source, destination, pane)
}

fn edit_moving_terminal(app: &mut Cagents, card: CardId, source: ChannelId) -> Pane {
    use super::super::chrome_ui::Action;
    drop(app.update_chrome(Action::Rename(card)));
    drop(app.update_chrome(Action::RenameChanged("Renamed while moving".into())));
    drop(app.update_chrome(Action::RenameSubmit));
    app.workspace.resize_viewport(1000.0);
    drop(app.update(Message::ResizeCard { card, width: 640.0 }));
    drop(app.update(Message::ResizeCardFinished(card)));
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        panic!("terminal");
    };
    let owner = session
        .continuity
        .live_proof()
        .filter(HostSessionProof::is_complete)
        .unwrap_or_else(|| HostSessionProof {
            host_session_id: "observed-move-host".into(),
            boot_id: "observed-move-boot".into(),
            host_incarnation: "observed-move-instance".into(),
        });
    let observed = (
        session.id,
        views::TerminalRemovalBasis::Worker {
            owner,
            instance: uuid::Uuid::new_v4(),
        },
    );
    // This is the durable descriptor boundary written by ResumeHintChanged.
    session.resume_hint = Some("observed-provider-session-while-moving".into());
    app.shared_views
        .terminal_source_workers
        .insert(card, observed);
    app.persist();
    let pane = app
        .local_view_panes(&Scope::Channel(source))
        .into_iter()
        .find(|pane| pane.id == card)
        .unwrap();
    assert_eq!(pane.user_title.as_deref(), Some("Renamed while moving"));
    assert_ne!(pane.width_fraction, 0.5);
    pane
}

fn moving_terminal_kind() -> CardKind {
    let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
    session.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("move-host"),
        boot_id: crate::model::BootId::new("move-boot"),
        host_incarnation: crate::model::HostIncarnation::new("move-incarnation"),
    };
    CardKind::Terminal(session)
}

fn assert_moving_metadata(app: &Cagents, expected: &Pane) {
    let card = app.card_state(expected.id).unwrap();
    assert_eq!(card.title, expected.title);
    assert_eq!(
        app.chrome_ui.user_titles.get(&expected.id),
        expected.user_title.as_ref()
    );
    assert_eq!(card.width_fraction, expected.width_fraction);
    assert_eq!(card.kind, expected.kind);
}

#[test]
fn accepted_channel_move_retargets_user_and_observed_descriptor_edits_before_projection() {
    use super::super::workspace_model_sync::ModelUpdate;
    for watcher_first in [false, true] {
        let kind = moving_terminal_kind();
        let (mut app, source, destination, pane) = channel_move_fixture_with_kind(Some(kind));
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation: transfer,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer");
        };
        let expected = edit_moving_terminal(&mut app, pane.id, source);
        let mut old = Vec::new();
        while let Ok(edit) = rx.try_recv() {
            if let ModelEdit::ChangeView {
                scope,
                operation,
                change: change @ (Change::Update { .. } | Change::UpdateTerminalSource { .. }),
                ..
            } = edit
            {
                assert_eq!(scope, Scope::Channel(source));
                old.push((operation, change));
            }
        }
        assert!(
            old.len() >= 3,
            "rename, resize and observed descriptor are separate real publications"
        );
        let mut model = app.workspace_model.clone();
        views::transfer(
            &mut model.views,
            &Scope::Channel(source),
            pane.id,
            &Scope::Channel(destination),
            None,
        )
        .unwrap();
        model.sequence += 1;
        model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(destination))
            .unwrap()
            .revision = 1;
        if watcher_first {
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            assert!(app.pane_transfer_pending(pane.id));
            assert_moving_metadata(&app, &expected);
        }
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(source),
            operation: Some(transfer),
            model: model.clone(),
        }));
        assert_moving_metadata(&app, &expected);
        assert!(!app.pane_transfer_pending(pane.id));
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == destination)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        let mut retried = Vec::new();
        while let Ok(edit) = rx.try_recv() {
            if let ModelEdit::ChangeView {
                scope,
                expected_revision,
                operation,
                change: change @ (Change::Update { .. } | Change::UpdateTerminalSource { .. }),
            } = edit
            {
                assert_eq!(scope, Scope::Channel(destination));
                assert_eq!(expected_revision, 1);
                assert!(!old.iter().any(|(previous, _)| *previous == operation));
                retried.push((operation, change));
            }
        }
        assert_eq!(
            retried.len(),
            old.len(),
            "watcher and transfer acknowledgment must not duplicate retries"
        );
        for (operation, change) in old {
            assert!(
                !model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(source))
                    .unwrap()
                    .apply(&change)
                    .unwrap()
            );
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(source),
                operation: Some(operation),
                model: model.clone(),
            }));
            assert_moving_metadata(&app, &expected);
            assert!(
                !app.shared_views.pending.is_empty(),
                "an old-scope acknowledgment cannot retire its retargeted intent"
            );
        }
        for (operation, change) in retried {
            model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(destination))
                .unwrap()
                .apply(&change)
                .unwrap();
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(destination),
                operation: Some(operation),
                model: model.clone(),
            }));
            assert_moving_metadata(&app, &expected);
        }
        assert!(app.shared_views.pending.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "settled updates do not publish a feedback loop"
        );
    }
}

#[test]
fn refused_channel_move_keeps_metadata_edits_in_the_original_scope() {
    use super::super::workspace_model_sync::ModelUpdate;
    let kind = moving_terminal_kind();
    let (mut app, source, destination, pane) = channel_move_fixture_with_kind(Some(kind));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.move_pane_to_channel(pane.id, destination));
    let ModelEdit::ChangeView {
        operation: transfer,
        ..
    } = rx.try_recv().unwrap()
    else {
        panic!("transfer");
    };
    let expected = edit_moving_terminal(&mut app, pane.id, source);
    let pending = app.shared_views.pending.clone();
    assert!(pending.len() >= 3);
    while rx.try_recv().is_ok() {}
    drop(
        app.apply_workspace_model_update(ModelUpdate::PaneTransferFailed {
            operation: transfer,
            error: "destination removed".into(),
        }),
    );
    assert_moving_metadata(&app, &expected);
    assert_eq!(app.shared_views.pending, pending);
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
    assert!(
        rx.try_recv().is_err(),
        "a refused transfer must not retarget or duplicate metadata operations"
    );
}

#[test]
fn channel_move_keeps_current_descriptor_and_independent_user_edits() {
    use super::super::workspace_model_sync::ModelUpdate;
    for replacement in [
        "session",
        "proof",
        "unproved-before",
        "unproved-current",
        "incomplete-both",
        "same-process-metadata",
        "other-current-field",
    ] {
        let mut kind = moving_terminal_kind();
        let CardKind::Terminal(session) = &mut kind else {
            panic!("terminal");
        };
        if replacement == "unproved-before" {
            session.continuity = TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: None,
            };
        } else if replacement == "incomplete-both" {
            let TerminalContinuity::Live { boot_id, .. } = &mut session.continuity else {
                panic!("proof");
            };
            *boot_id = crate::model::BootId::new("");
        }
        let (mut app, source, destination, pane) = channel_move_fixture_with_kind(Some(kind));
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation: transfer,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer");
        };
        let mut expected = edit_moving_terminal(&mut app, pane.id, source);
        while rx.try_recv().is_ok() {}
        let mut model = app.workspace_model.clone();
        views::transfer(
            &mut model.views,
            &Scope::Channel(source),
            pane.id,
            &Scope::Channel(destination),
            None,
        )
        .unwrap();
        let destination_view = model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(destination))
            .unwrap();
        let current = &mut destination_view.panes[0].kind;
        let CardKind::Terminal(session) = current else {
            panic!("terminal");
        };
        match replacement {
            "session" => session.id = TerminalId::new(),
            "proof" => {
                let TerminalContinuity::Live {
                    host_incarnation, ..
                } = &mut session.continuity
                else {
                    panic!("proof");
                };
                *host_incarnation = crate::model::HostIncarnation::new("replacement-incarnation");
            }
            "unproved-current" => {
                session.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: session.launch.clone(),
                    last_exit: None,
                }
            }
            _ => {}
        }
        if replacement == "other-current-field" {
            session.transcript_path = Some("/authoritative-current/transcript".into());
        } else {
            session.resume_hint = Some("authoritative-current-session".into());
        }
        expected.kind = current.clone();
        model.sequence += 1;
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                scope: Scope::Channel(source),
                operation: Some(transfer),
                model: model.clone(),
            })),
        );
        assert_moving_metadata(&app, &expected);
        assert!(
            app.notices.recent().any(|entry| entry
                .notice
                .message
                .contains("incoming session update could not be verified")),
            "{replacement}"
        );
        let mut retried = Vec::new();
        while let Ok(edit) = rx.try_recv() {
            if let ModelEdit::ChangeView {
                scope,
                operation,
                change:
                    Change::Update {
                        before,
                        pane: after,
                    },
                ..
            } = edit
            {
                assert_eq!(scope, Scope::Channel(destination));
                assert_eq!(
                    before.kind, after.kind,
                    "an old actor must not overwrite its replacement: {replacement}"
                );
                retried.push((
                    operation,
                    Change::Update {
                        before,
                        pane: after,
                    },
                ));
            }
        }
        assert!(
            !retried.is_empty(),
            "title and width intentions still need acknowledgment"
        );
        for (operation, change) in retried {
            model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(destination))
                .unwrap()
                .apply(&change)
                .unwrap();
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(destination),
                operation: Some(operation),
                model: model.clone(),
            }));
            assert_moving_metadata(&app, &expected);
        }
        assert!(app.shared_views.pending.is_empty());
    }
}

#[test]
fn channel_move_refuses_stale_start_and_scope_fields_without_a_kind_delta() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, source, destination, pane) =
        channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.move_pane_to_channel(pane.id, destination));
    let ModelEdit::ChangeView {
        operation: transfer,
        ..
    } = rx.try_recv().unwrap()
    else {
        panic!("transfer");
    };
    // Exercise the pending Update boundary independently of whether a
    // particular producer currently emits both source-sensitive deltas.
    let mut before = pane.clone();
    before.pending_terminal_start = true;
    before.scope = Some("/previous-scope".into());
    let mut desired = before.clone();
    desired.pending_terminal_start = false;
    desired.scope = Some("/stale-scope".into());
    desired.title = "Preserved presentation edit".into();
    app.shared_views.pending.push((
        uuid::Uuid::new_v4(),
        Scope::Channel(source),
        Change::Update {
            before,
            pane: desired.clone(),
        },
    ));
    let (operation, scope, change) = app.shared_views.pending.last().unwrap().clone();
    app.publish_model_edit(ModelEdit::ChangeView {
        scope,
        expected_revision: 17,
        operation,
        change,
    });
    while rx.try_recv().is_ok() {}
    let mut model = app.workspace_model.clone();
    // Metadata adoption must not publish the old pane before projection.
    model
        .channels
        .iter_mut()
        .find(|channel| channel.id == source)
        .unwrap()
        .title = "Renamed source while moving".into();
    views::transfer(
        &mut model.views,
        &Scope::Channel(source),
        pane.id,
        &Scope::Channel(destination),
        None,
    )
    .unwrap();
    let current = &mut model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap()
        .panes[0];
    let CardKind::Terminal(session) = &mut current.kind else {
        panic!("terminal");
    };
    session.id = TerminalId::new();
    current.pending_terminal_start = true;
    current.scope = Some("/authoritative-scope".into());
    let expected = current.clone();
    model.sequence += 1;
    drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(source),
        operation: Some(transfer),
        model: model.clone(),
    }));
    let ModelEdit::ChangeView { scope, change, .. } = rx.try_recv().unwrap() else {
        panic!("presentation retry");
    };
    assert_eq!(scope, Scope::Channel(destination));
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .title,
        "Renamed source while moving"
    );
    let Change::Update {
        before,
        pane: after,
    } = &change
    else {
        panic!("update");
    };
    assert_eq!(before.kind, after.kind);
    assert_eq!(before.pending_terminal_start, after.pending_terminal_start);
    assert_eq!(before.scope, after.scope);
    let destination = model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap();
    destination.apply(&change).unwrap();
    assert_eq!(destination.panes[0].title, desired.title);
    assert_eq!(destination.panes[0].kind, expected.kind);
    assert_eq!(
        destination.panes[0].pending_terminal_start,
        expected.pending_terminal_start
    );
    assert_eq!(destination.panes[0].scope, expected.scope);
    assert!(app.notices.recent().any(|entry| {
        entry
            .notice
            .message
            .contains("incoming session update could not be verified")
    }));
    let unexpected = rx.try_recv();
    assert!(
        unexpected.is_err(),
        "unexpected publication: {unexpected:?}"
    );
}

#[test]
fn transferred_updates_cannot_acquire_missing_or_foreign_source_authority() {
    use super::super::workspace_model_sync::ModelUpdate;
    for control in ["uncaptured", "foreign-watch-first", "foreign-registry"] {
        let (mut app, source, destination, pane) = channel_move_fixture();
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        drop(app.move_pane_to_channel(pane.id, destination));
        let ModelEdit::ChangeView {
            operation: transfer,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("transfer");
        };
        let operation = uuid::Uuid::new_v4();
        let mut desired = pane.clone();
        desired.title = "Unverified earlier title".into();
        let change = Change::Update {
            before: pane.clone(),
            pane: desired,
        };
        app.shared_views
            .pending
            .push((operation, Scope::Channel(source), change.clone()));
        if control != "uncaptured" {
            app.publish_model_edit(ModelEdit::ChangeView {
                scope: Scope::Channel(source),
                expected_revision: 17,
                operation,
                change,
            });
            while rx.try_recv().is_ok() {}
        }
        let mut model = app.workspace_model.clone();
        views::transfer(
            &mut model.views,
            &Scope::Channel(source),
            pane.id,
            &Scope::Channel(destination),
            None,
        )
        .unwrap();
        if control == "foreign-watch-first" {
            for channel in &mut model.channels {
                channel.repository = "/foreign/.git".into();
            }
        } else if control == "foreign-registry" {
            app.shared_views.source_bases.get_mut(&operation).unwrap().0 = uuid::Uuid::new_v4();
        }
        model.sequence += 1;
        app.workspace_model = model.clone();
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(source),
            operation: Some(transfer),
            model,
        }));
        assert!(
            rx.try_recv().is_err(),
            "an unknown source must not gain a fresh receipt: {control}"
        );
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, _, _)| *id == operation)
        );
        assert!(!app.shared_views.source_bases.contains_key(&operation));
        assert_eq!(app.card_state(pane.id).unwrap().title, pane.title);
        assert!(app.notices.recent().any(|entry| {
            entry
                .notice
                .message
                .contains("incoming session update could not be verified")
        }));
    }
}

/// Another window made the same session update, but this one also renamed
/// the pane. Quiet is only right while the name is still on its way.
#[test]
fn losing_a_session_update_race_stays_quiet_only_if_its_rename_survives() {
    use super::super::workspace_model_sync::ModelUpdate;
    for can_retry in [true, false] {
        let (mut app, source, _, before) = channel_move_fixture();
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        let operation = uuid::Uuid::new_v4();
        let mut desired = before.clone();
        desired.title = "Renamed here".into();
        desired.scope = Some("/same-new-source".into());
        let change = Change::Update {
            before: before.clone(),
            pane: desired.clone(),
        };
        app.shared_views
            .pending
            .push((operation, Scope::Channel(source), change.clone()));
        if can_retry {
            app.publish_model_edit(ModelEdit::ChangeView {
                scope: Scope::Channel(source),
                expected_revision: 17,
                operation,
                change: change.clone(),
            });
            rx.try_recv().unwrap();
        }
        let mut model = app.workspace_model.clone();
        model.views[0].panes[0].scope = desired.scope.clone();
        model.sequence += 1;
        drop(
            app.apply_workspace_model_update(ModelUpdate::ViewConflicted {
                scope: Scope::Channel(source),
                operation,
                change,
                model,
                error: "The pane source changed".into(),
            }),
        );

        let warned = app
            .notices
            .recent()
            .any(|entry| entry.notice.message.contains("changed elsewhere"));
        assert_eq!(warned, !can_retry);
        if can_retry {
            let ModelEdit::ChangeView { change, .. } = rx.try_recv().unwrap() else {
                panic!("presentation retry");
            };
            let Change::Update { pane, .. } = change else {
                panic!("update");
            };
            assert_eq!(pane.title, desired.title);
        } else {
            assert!(rx.try_recv().is_err());
        }
    }
}

#[test]
fn a_source_conflict_refuses_session_intent_and_reissues_only_safe_presentation() {
    use super::super::workspace_model_sync::ModelUpdate;
    for control in [
        "current",
        "missing",
        "foreign-project",
        "foreign-watch-first",
        "uncaptured",
        "later-title",
    ] {
        let (mut app, source, _, before) = channel_move_fixture();
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        let operation = uuid::Uuid::new_v4();
        let mut desired = before.clone();
        desired.title = "Earlier captured title".into();
        desired.user_title = Some("User title".into());
        desired.width_fraction = 0.8;
        desired.pending_terminal_start = true;
        desired.scope = Some("/stale-source".into());
        let change = Change::Update {
            before: before.clone(),
            pane: desired.clone(),
        };
        app.shared_views
            .pending
            .push((operation, Scope::Channel(source), change.clone()));
        if control != "uncaptured" {
            app.publish_model_edit(ModelEdit::ChangeView {
                scope: Scope::Channel(source),
                expected_revision: 17,
                operation,
                change: change.clone(),
            });
            assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
                operation: queued, .. } if queued == operation));
        }
        let later_operation = uuid::Uuid::new_v4();
        if control == "later-title" {
            let mut later = before.clone();
            later.title = "Later user title".into();
            app.shared_views.pending.push((
                later_operation,
                Scope::Channel(source),
                Change::Update {
                    before: before.clone(),
                    pane: later,
                },
            ));
        }
        let mut model = app.workspace_model.clone();
        let current = &mut model.views[0].panes[0];
        let CardKind::Browser(browser) = &mut current.kind else {
            panic!("browser");
        };
        browser.current_url = "https://authoritative.example".parse().unwrap();
        current.scope = Some("/authoritative-source".into());
        let expected = current.clone();
        if control == "missing" {
            model.views[0].panes.clear();
        }
        if matches!(control, "foreign-project" | "foreign-watch-first") {
            model
                .channels
                .iter_mut()
                .find(|channel| channel.id == source)
                .unwrap()
                .repository = "/foreign/.git".into();
        }
        model.sequence += 1;
        if control == "foreign-watch-first" {
            // A preceding watcher must not redefine this operation's
            // captured logical source before its conflict reply arrives.
            app.workspace_model = model.clone();
        }
        drop(
            app.apply_workspace_model_update(ModelUpdate::ViewConflicted {
                scope: Scope::Channel(source),
                operation,
                change,
                model: model.clone(),
                error: "The pane source changed".into(),
            }),
        );
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, _, _)| *id == operation)
        );
        assert!(
            app.notices
                .recent()
                .any(|entry| entry.notice.message.contains("changed elsewhere"))
        );
        if matches!(
            control,
            "missing" | "foreign-project" | "foreign-watch-first" | "uncaptured"
        ) {
            assert!(
                rx.try_recv().is_err(),
                "refusal cannot rename a missing or foreign source"
            );
            assert!(!app.shared_views.source_bases.contains_key(&operation));
            continue;
        }
        let ModelEdit::ChangeView {
            operation: retry,
            change,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("presentation retry");
        };
        assert_ne!(retry, operation);
        let Change::Update { before, pane } = &change else {
            panic!("update");
        };
        assert_eq!(before.kind, expected.kind);
        assert_eq!(pane.kind, expected.kind);
        assert_eq!(before.pending_terminal_start, pane.pending_terminal_start);
        assert_eq!(before.scope, pane.scope);
        assert_eq!(pane.user_title, desired.user_title);
        assert_eq!(pane.width_fraction, desired.width_fraction);
        let expected_title = if control == "later-title" {
            &expected.title
        } else {
            &desired.title
        };
        assert_eq!(&pane.title, expected_title);
        // A delayed old receipt cannot retire the independent new intent.
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(source),
            operation: Some(operation),
            model: model.clone(),
        }));
        assert!(
            app.shared_views
                .pending
                .iter()
                .any(|(id, _, _)| *id == retry)
        );
        if control == "later-title" {
            assert!(
                app.shared_views
                    .pending
                    .iter()
                    .any(|(id, _, _)| *id == later_operation)
            );
            assert_eq!(app.card_state(pane.id).unwrap().title, "Later user title");
        }
        model.views[0].apply(&change).unwrap();
        model.sequence += 1;
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(source),
            operation: Some(retry),
            model,
        }));
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, _, _)| *id == retry)
        );
        assert!(!app.shared_views.source_bases.contains_key(&retry));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn channel_move_does_not_retarget_another_panes_pending_update() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, source, destination, pane) = channel_move_fixture();
    let mut other = pane.clone();
    other.id = CardId::new();
    let CardKind::Browser(session) = &mut other.kind else {
        panic!("browser");
    };
    session.id = BrowserId::new();
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .panes
        .push(other.clone());
    drop(app.reconcile_shared_views());
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.move_pane_to_channel(pane.id, destination));
    let ModelEdit::ChangeView {
        operation: transfer,
        ..
    } = rx.try_recv().unwrap()
    else {
        panic!("transfer");
    };
    let unrelated = uuid::Uuid::new_v4();
    let mut renamed = other.clone();
    renamed.title = "Keep another pane's edit".into();
    let intent = (
        unrelated,
        Scope::Channel(source),
        Change::Update {
            before: other.clone(),
            pane: renamed,
        },
    );
    app.shared_views.pending.push(intent.clone());
    let mut model = app.workspace_model.clone();
    views::transfer(
        &mut model.views,
        &Scope::Channel(source),
        pane.id,
        &Scope::Channel(destination),
        None,
    )
    .unwrap();
    model.sequence += 1;
    drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(source),
        operation: Some(transfer),
        model,
    }));
    assert_eq!(app.shared_views.pending, vec![intent]);
    assert_eq!(
        app.card_state(other.id).unwrap().title,
        "Keep another pane's edit"
    );
    assert!(
        rx.try_recv().is_err(),
        "unrelated intentions retain their existing operation receipt"
    );
}

#[test]
fn active_repository_save_blocks_channel_commands_picker_and_direct_moves() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    assert!(app.pane_has_move_destination(pane.id));
    let (runtime, _) = RepositoryRuntime::post_persist_failure_fixture(pane.id);
    let kind = CardKind::Repository(runtime.descriptor.clone());
    app.card_state_mut(pane.id).unwrap().kind = kind.clone();
    app.workspace_model
        .views
        .iter_mut()
        .flat_map(|view| &mut view.panes)
        .find(|candidate| candidate.id == pane.id)
        .unwrap()
        .kind = kind;
    app.cards
        .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
    assert!(app.focused_repository_is_saving());
    assert!(!app.pane_has_move_destination(pane.id));
    assert!(app.pane_move_destinations(pane.id).is_empty());
    drop(app.move_pane_to_channel(pane.id, destination));
    drop(app.update(Message::MovePaneToChannel {
        card: pane.id,
        channel: destination,
    }));
    drop(app.update(Message::PaneDropToChannel {
        generation: app.workspace_view_generation,
        card: pane.id,
        channel: destination,
    }));
    drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
    assert!(!app.palette.is_open());
    assert!(rx.try_recv().is_err());
    assert!(!app.pane_transfer_pending(pane.id));
    assert_eq!(app.durable.active_channel, Some(source));
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
}

#[test]
fn asynchronous_terminal_close_blocks_moves_until_its_actual_cancel() {
    use super::super::terminal_close::CloseOutcome;
    let (mut app, source, destination, pane) =
        channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    // Starting the real application close reserves its asynchronous owner,
    // while the pane and runtime remain open and no dialog is visible yet.
    drop(app.begin_card_close(pane.id));
    assert!(app.terminal_close_pending(pane.id));
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert!(!app.modal_is_open());
    assert!(app.pane_move_destinations(pane.id).is_empty());
    drop(app.update(Message::MovePaneToChannel {
        card: pane.id,
        channel: destination,
    }));
    drop(app.update(Message::PaneDropToChannel {
        generation: app.workspace_view_generation,
        card: pane.id,
        channel: destination,
    }));
    assert!(
        rx.try_recv().is_err(),
        "close cannot be overtaken by a transfer"
    );
    assert!(!app.pane_transfer_pending(pane.id));
    let intent = app.terminal_close.as_ref().unwrap();
    let (terminal, proof, generation) = (intent.terminal, intent.proof.clone(), intent.generation);
    drop(app.terminal_close_finished(pane.id, terminal, proof, generation, CloseOutcome::Active));
    assert!(app.modal_is_open());
    assert!(app.pane_move_destinations(pane.id).is_empty());
    drop(app.cancel_terminal_close());
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
        scope: Scope::Channel(from), change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
    } if from == source && id == pane.id && to == destination));
}

#[test]
fn focus_handoff_close_keeps_its_source_until_commit() {
    let (mut app, _, destination, pane) = channel_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.request_card_removal(pane.id));
    assert_eq!(app.workspace.pending_close().unwrap().unmounting, pane.id);
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert!(app.pane_move_destinations(pane.id).is_empty());
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(rx.try_recv().is_err());
    assert!(!app.pane_transfer_pending(pane.id));
}

#[test]
fn channel_move_waits_for_host_acknowledgment_and_preserves_the_pane() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let kind = app.card_state(pane.id).unwrap().kind.clone();
    let runtime = app.cards.get(&pane.id).unwrap() as *const CardRuntime;
    drop(app.move_pane_to_channel(pane.id, destination));
    let ModelEdit::ChangeView {
        scope,
        expected_revision,
        operation,
        change,
    } = rx.try_recv().unwrap()
    else {
        panic!("a transfer request");
    };
    assert_eq!(scope, Scope::Channel(source));
    assert_eq!(expected_revision, 17);
    assert!(
        matches!(change, Change::Transfer { id, destination: Scope::Channel(to), before: None }
        if id == pane.id && to == destination)
    );
    assert!(
        !app.can_move_pane_to_channel(pane.id, destination),
        "duplicate pending move is disabled"
    );
    drop(app.begin_card_close(pane.id));
    assert!(
        app.card_state(pane.id).unwrap().existence.is_open(),
        "closing against the old scope must wait for the transfer acknowledgment"
    );
    assert!(
        app.status
            .as_ref()
            .unwrap()
            .message
            .contains("moving to a channel")
    );
    assert!(
        rx.try_recv().is_err(),
        "a pending move must not publish a stale-scope removal"
    );
    drop(app.reconcile_shared_views());
    assert!(
        app.durable
            .channels
            .iter()
            .find(|c| c.id == source)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
    assert!(
        app.durable
            .channels
            .iter()
            .find(|c| c.id == destination)
            .unwrap()
            .cards
            .is_empty()
    );
    views::transfer(
        &mut app.workspace_model.views,
        &Scope::Channel(source),
        pane.id,
        &Scope::Channel(destination),
        None,
    )
    .unwrap();
    app.acknowledge_view_edit(scope, Some(operation));
    drop(app.reconcile_shared_views());
    assert_eq!(app.card_state(pane.id).unwrap().kind, kind);
    assert_eq!(
        app.cards.get(&pane.id).unwrap() as *const CardRuntime,
        runtime
    );
    assert!(
        app.durable
            .channels
            .iter()
            .find(|c| c.id == destination)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
}

#[test]
fn channel_move_rejects_other_projects_and_reports_host_refusal_without_rehoming() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    app.workspace_model
        .channels
        .iter_mut()
        .find(|c| c.id == destination)
        .unwrap()
        .repository = PathBuf::from("/other/.git");
    assert!(!app.can_move_pane_to_channel(pane.id, destination));
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(rx.try_recv().is_err());
    assert!(!app.can_move_pane_to_channel(CardId::new(), destination));
    app.workspace_model
        .channels
        .iter_mut()
        .find(|c| c.id == destination)
        .unwrap()
        .repository = PathBuf::from("/repo/.git");
    drop(app.move_pane_to_channel(pane.id, destination));
    let ModelEdit::ChangeView { operation, .. } = rx.try_recv().unwrap() else {
        panic!("transfer");
    };
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::PaneTransferFailed {
            operation,
            error: "destination no longer exists".into(),
        },
    ));
    assert!(app.shared_views.requested_transfers.is_empty());
    assert!(
        app.status
            .as_ref()
            .unwrap()
            .message
            .contains("destination no longer exists")
    );
    assert!(
        app.durable
            .channels
            .iter()
            .find(|c| c.id == source)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
    assert!(app.can_move_pane_to_channel(pane.id, destination));
}

#[test]
fn refused_optimistic_transfer_restores_membership_without_erasing_other_intentions() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let refused = uuid::Uuid::new_v4();
    let rename = uuid::Uuid::new_v4();
    let mut renamed = pane.clone();
    renamed.title = "Keep my pending rename".into();
    app.shared_views.pending.push((
        refused,
        Scope::Channel(source),
        Change::Transfer {
            id: pane.id,
            destination: Scope::Channel(destination),
            before: None,
        },
    ));
    app.shared_views.pending.push((
        rename,
        Scope::Channel(source),
        Change::Update {
            before: pane.clone(),
            pane: renamed,
        },
    ));
    drop(app.reconcile_shared_views());
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == destination)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::ViewRefused {
            scope: Scope::Channel(source),
            operation: refused,
            error: "destination was removed".into(),
        },
    ));
    assert_eq!(app.shared_views.pending.len(), 1);
    assert_eq!(app.shared_views.pending[0].0, rename);
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == destination)
            .unwrap()
            .cards
            .is_empty()
    );
    assert_eq!(
        app.card_state(pane.id).unwrap().title,
        "Keep my pending rename"
    );
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
}

#[test]
fn worktree_panes_can_choose_authoritative_unvisited_channels() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let worktree = Scope::Worktree {
        repository: PathBuf::from("/repo/.git"),
        path: PathBuf::from("/repo/worktree"),
    };
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .scope = worktree.clone();
    let durable = Arc::make_mut(&mut app.durable);
    durable.active_channel = None;
    durable.channels.clear();
    durable
        .card_scopes
        .insert(pane.id, PathBuf::from("/repo/worktree"));
    let destinations = app.pane_move_destinations(pane.id);
    assert!(
        destinations.iter().any(|channel| channel.id == destination),
        "authoritative channels remain selectable without a local channel descriptor"
    );
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
        scope, expected_revision: 17, change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
    } if scope == worktree && id == pane.id && to == destination));
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
}

#[test]
fn move_palette_filters_destinations_and_waits_for_host_acceptance() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    app.workspace_model
        .channels
        .iter_mut()
        .find(|channel| channel.id == destination)
        .unwrap()
        .title = "Destination needle".into();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
    assert!(app.palette.is_open());
    drop(app.update_palette(super::super::palette_ui::Action::Query("needle".into())));
    drop(app.update_palette(super::super::palette_ui::Action::Submit));
    assert!(
        !app.palette.is_open(),
        "dismiss before dispatch so the move passes the modal guard"
    );
    assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
        scope: Scope::Channel(from), change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
    } if from == source && id == pane.id && to == destination));
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards
            .contains(&pane.id),
        "choosing a destination must not optimistically detach the live pane"
    );
    assert!(app.pane_transfer_pending(pane.id));
}

#[test]
fn move_palette_keeps_selected_destination_across_live_ranking_changes() {
    for query in [false, true] {
        let (mut app, source, destination, pane) = channel_move_fixture();
        app.workspace_model
            .channels
            .iter_mut()
            .find(|channel| channel.id == destination)
            .unwrap()
            .title = "Other needle destination".into();
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
        if query {
            drop(app.update_palette(super::super::palette_ui::Action::Query("needle".into())));
        }
        let mut stronger = app.workspace_model.channel(destination).unwrap().clone();
        stronger.id = ChannelId::new();
        stronger.title = "needle first".into();
        stronger.ordinal += 1;
        app.workspace_model.channels.push(stronger);
        drop(app.update_palette(super::super::palette_ui::Action::Submit));
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            scope: Scope::Channel(from), change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
        } if from == source && id == pane.id && to == destination));
        assert!(app.pane_transfer_pending(pane.id));
    }
}

#[test]
fn move_palette_does_not_substitute_another_live_destination_when_selected_one_disappears() {
    let (mut app, _, destination, pane) = channel_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.update_palette(super::super::palette_ui::Action::MovePane(pane.id)));
    let mut replacement = app.workspace_model.channel(destination).unwrap().clone();
    replacement.id = ChannelId::new();
    app.workspace_model
        .channels
        .retain(|channel| channel.id != destination);
    app.workspace_model.channels.push(replacement);
    drop(app.update_palette(super::super::palette_ui::Action::Submit));
    assert!(rx.try_recv().is_err());
    assert!(!app.pane_transfer_pending(pane.id));
    assert!(app.palette.is_open());
}

#[test]
fn a_queued_channel_drop_from_an_old_workspace_is_ignored() {
    let (mut app, _, destination, pane) = channel_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.update(Message::PaneDropToChannel {
        generation: app.workspace_view_generation.wrapping_add(1),
        card: pane.id,
        channel: destination,
    }));
    assert!(rx.try_recv().is_err());
    assert!(!app.pane_transfer_pending(pane.id));
}

#[test]
fn repository_scope_closure_keeps_all_owners_of_a_shared_path() {
    let (mut app, source, _, pane) = channel_move_fixture();
    let first = PathBuf::from("/repo/.git");
    let second = PathBuf::from("/other/.git");
    let path = PathBuf::from("/repo/shared-tree");
    let first_project = app.durable.channels[0].project_id;
    for (id, repository) in [
        (first_project, first.clone()),
        (ProjectId::new(), second.clone()),
    ] {
        app.sidebar_model.projects.push(SidebarProject::new(
            id,
            RepositoryId::new(),
            "repo".into(),
            repository,
            vec![],
            vec![],
        ));
    }
    let a = Scope::Worktree {
        repository: first,
        path: path.clone(),
    };
    let b = Scope::Worktree {
        repository: second.clone(),
        path: path.clone(),
    };
    // Both scopes receive this path's members in local_views. Put `a`
    // last to catch a path index that keeps only the last authority.
    app.workspace_model
        .views
        .extend([View::empty(b.clone()), View::empty(a.clone())]);
    app.known_worktree_roots.insert(path.clone());
    let durable = Arc::make_mut(&mut app.durable);
    for channel in &mut durable.channels {
        channel.cards.retain(|id| *id != pane.id);
    }
    durable.card_scopes.insert(pane.id, path);
    let selected = app.repository_view_scopes(&second, &HashSet::new());
    assert!(selected.contains(&a));
    assert!(selected.contains(&b));
    assert!(selected.contains(&Scope::Channel(source)));
}

#[test]
fn repository_scope_closure_follows_adopted_scopes_and_pending_transfers() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    app.shared_views.pending.push((
        uuid::Uuid::new_v4(),
        Scope::Channel(source),
        Change::Transfer {
            id: pane.id,
            destination: Scope::Channel(destination),
            before: None,
        },
    ));
    let selected = app.repository_view_scopes(
        Path::new("/unrelated"),
        &HashSet::from([Scope::Channel(destination)]),
    );
    assert!(selected.contains(&Scope::Channel(source)));
    assert!(selected.contains(&Scope::Channel(destination)));
}

#[test]
fn pending_insert_keeps_raw_alias_dependency_without_retiring_overlay_ownership() {
    let (mut app, source, destination, alias) = channel_move_fixture();
    let mut canonical = alias.clone();
    canonical.id = CardId::new();
    app.workspace_model.views[0].panes.clear();
    app.workspace_model.views[1].panes.push(canonical.clone());
    app.shared_views.pending.push((
        uuid::Uuid::new_v4(),
        Scope::Channel(source),
        Change::Insert {
            pane: alias.clone(),
            before: None,
        },
    ));
    let selected = app.repository_view_scopes(
        Path::new("/unrelated"),
        &HashSet::from([Scope::Channel(destination)]),
    );
    assert!(selected.contains(&Scope::Channel(source)));
    let mut overlay = app.workspace_model.views.clone();
    overlay[0].apply(&app.shared_views.pending[0].2).unwrap();
    let owned = overlay
        .iter()
        .flat_map(|view| view.panes.iter().map(|pane| pane.id))
        .collect::<HashSet<_>>();

    app.prepare_shared_aliases(&overlay, &owned);

    assert!(app.card_state(alias.id).unwrap().existence.is_open());
    assert_eq!(app.durable.channels[0].cards, [alias.id]);
    assert!(
        !app.card_state(canonical.id)
            .is_some_and(|card| card.existence.is_open())
    );
    assert_eq!(app.shared_views.pending.len(), 1);
}

#[test]
fn repository_path_change_uses_fresh_authority_without_rewriting_its_previous_scope() {
    let (mut app, _, _, mut old) = fixture();
    let project = app.durable.channels[0].project_id;
    let old_repository = PathBuf::from("/repo/.git");
    let old_path = PathBuf::from("/repo/previous-root");
    let new_repository = PathBuf::from("/moved/.git");
    let new_path = PathBuf::from("/moved/new-root");
    let old_scope = Scope::Worktree {
        repository: old_repository.clone(),
        path: old_path.clone(),
    };
    let new_scope = Scope::Worktree {
        repository: new_repository.clone(),
        path: new_path.clone(),
    };
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        old_repository.clone(),
        vec![crate::sidebar::SidebarWorktree::from_durable(
            crate::model::WorktreeId::new(),
            old_path.clone(),
            None,
        )],
        vec![],
    ));
    app.known_worktree_roots.insert(old_path.clone());
    let durable = Arc::make_mut(&mut app.durable);
    durable.active_channel = None;
    durable.cards.push(CardState {
        id: old.id,
        generation: 0,
        title: old.title.clone(),
        width_fraction: old.width_fraction,
        existence: Existence::Open,
        kind: old.kind.clone(),
    });
    durable.card_scopes.insert(old.id, old_path.clone());
    app.cards.insert(old.id, CardRuntime::Completed);
    old.scope = Some(old_path.clone());
    let mut view = View::empty(old_scope.clone());
    view.panes.push(old.clone());
    app.workspace_model.views = vec![view];
    app.workspace_model
        .projects
        .push(crate::workspace_model::ModelProject {
            repository: old_repository.clone(),
            name: "Historical host repository".into(),
            kind: Default::default(),
            ordinal: 0,
        });
    drop(app.reconcile_shared_views_for_repository(&old_repository, &HashSet::new()));
    let baseline = app.shared_views.baseline[&old_scope].clone();

    // A later load changes this local row's path. The host still owns the
    // previous view; its distinct resource must not be refiled implicitly.
    app.sidebar_model.projects[0].repository = new_repository.clone();
    app.sidebar_model.projects[0].worktrees = vec![crate::sidebar::SidebarWorktree::from_durable(
        crate::model::WorktreeId::new(),
        new_path.clone(),
        None,
    )];
    app.known_worktree_roots.insert(new_path.clone());
    let mut new = old.clone();
    new.id = CardId::new();
    new.scope = Some(new_path.clone());
    if let CardKind::Browser(browser) = &mut new.kind {
        browser.id = BrowserId::new();
    }
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(CardState {
        id: new.id,
        generation: 0,
        title: new.title.clone(),
        width_fraction: new.width_fraction,
        existence: Existence::Open,
        kind: new.kind.clone(),
    });
    durable.card_scopes.insert(new.id, new_path.clone());
    app.cards.insert(new.id, CardRuntime::Completed);
    app.workspace_model
        .views
        .push(View::empty(new_scope.clone()));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;

    drop(app.reconcile_shared_views_for_repository(&new_repository, &HashSet::new()));

    assert_eq!(app.shared_views.baseline[&old_scope], baseline);
    assert_eq!(app.card_state(old.id).unwrap().kind, old.kind);
    assert_eq!(app.durable.card_scopes.get(&old.id), Some(&old_path));
    assert!(app.card_state(old.id).unwrap().existence.is_open());
    let scopes = app.local_view_scopes();
    assert!(scopes.contains(&new_scope));
    assert!(!scopes.contains(&Scope::Worktree {
        repository: new_repository,
        path: old_path
    }));
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(edits.iter().any(|edit|
        matches!(edit, ModelEdit::SeedView { scope, panes, .. } if scope == &new_scope && panes.iter().any(|pane| pane.id == new.id))));
    assert!(!edits.iter().any(|edit|
        matches!(edit, ModelEdit::SeedView { scope, .. } | ModelEdit::ChangeView { scope, .. } if scope == &old_scope)));
}

#[test]
fn repository_loads_project_only_their_affected_views() {
    let (mut app, unrelated, _, pane) = channel_move_fixture();
    let baseline = app.shared_views.baseline[&Scope::Channel(unrelated)].clone();
    let pending = (
        uuid::Uuid::new_v4(),
        Scope::Channel(unrelated),
        Change::Remove { id: CardId::new() },
    );
    app.shared_views.pending.push(pending.clone());
    let mut loaded = Vec::new();
    for index in 0..6 {
        let id = ProjectId::new();
        let repository = PathBuf::from(format!("/repositories/project-{index}"));
        app.sidebar_model.projects.push(SidebarProject::new(
            id,
            RepositoryId::new(),
            format!("project-{index}"),
            repository.clone(),
            vec![],
            vec![],
        ));
        app.repository_generations.insert(id, 1);
        app.workspace_model
            .projects
            .push(crate::workspace_model::ModelProject {
                repository: repository.clone(),
                name: format!("project-{index}"),
                kind: Default::default(),
                ordinal: index,
            });
        for view in 0..8 {
            let scope = Scope::Worktree {
                repository: repository.clone(),
                path: repository.join(format!("tree-{view}")),
            };
            app.shared_views.initialized.insert(scope.clone());
            app.workspace_model.views.push(View::empty(scope));
        }
        loaded.push(LoadedRepository {
            project: ProjectState {
                kind: Default::default(),
                id,
                generation: 0,
                name: format!("project-{index}"),
                repository,
                existence: Existence::Open,
            },
            repository: None,
            worktrees: vec![],
            github: GitHubAvailability::NotAssociated,
            review_repository: None,
            head_repositories: HashMap::new(),
        });
    }
    app.shared_views.view_projection_count.set(0);
    app.shared_views.topology_build_count.set(0);
    for loaded in loaded {
        drop(app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: loaded.project.id,
            generation: 1,
            result: Ok(Box::new(loaded)),
        }));
    }
    assert_eq!(
        app.shared_views.topology_build_count.get(),
        0,
        "a run of background loads waits for one reconciliation"
    );
    assert_eq!(
        app.pending_repository_reconciliation.len(),
        6,
        "each load joins the pending reconciliation"
    );
    drop(app.update(Message::RepositoryReconciliationDue));
    assert!(app.pending_repository_reconciliation.is_empty());
    assert_eq!(
        app.shared_views.topology_build_count.get(),
        1,
        "the loads share one pass, which builds its immutable root and authority indexes once"
    );
    assert_eq!(
        app.shared_views.view_projection_count.get(),
        6 * 8,
        "loading repositories must not project any other repository's views"
    );
    assert!(
        app.durable
            .channels
            .iter()
            .any(|channel| channel.id == unrelated && channel.cards.contains(&pane.id))
    );
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert_eq!(
        app.shared_views.baseline[&Scope::Channel(unrelated)],
        baseline
    );
    assert!(
        app.shared_views.pending.contains(&pending),
        "an unrelated operation keeps its receipt and scope"
    );
}

/// A desktop-scale inventory: 30 channels holding 122 open panes, 433
/// closed historical cards and 1,218 historical worktree scopes.
fn desktop_inventory_fixture() -> (Cagents, crate::workspace_model::WorkspaceModel) {
    let (mut app, _, _, template) = fixture();
    let repository = PathBuf::from("/repo/.git");
    let project = app.durable.channels[0].project_id;
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        repository.clone(),
        vec![],
        vec![],
    ));
    let mut model = crate::workspace_model::WorkspaceModel {
        identity: app.workspace_model.identity,
        sequence: 17,
        ..Default::default()
    };
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.clear();
    durable.cards.clear();
    durable.card_scopes.clear();
    durable.active_channel = None;
    for index in 0..30 {
        let id = ChannelId::new();
        durable.channels.push(ProjectChannel {
            id,
            project_id: project,
            title: format!("Channel {index}"),
            title_source: Default::default(),
            ordinal: index,
            shared: true,
            cards: vec![],
            focused: None,
            checkout: None,
        });
        model.channels.push(crate::workspace_model::ModelChannel {
            id,
            repository: repository.clone(),
            title: format!("Channel {index}"),
            title_source: Default::default(),
            ordinal: index,
            checkout: None,
        });
        let scope = Scope::Channel(id);
        model.views.push(View::empty(scope.clone()));
        app.shared_views.initialized.insert(scope);
    }
    // The desktop has many historical cards and scopes, not 555 live
    // providers. Only these 122 panes are open shared descriptors.
    for index in 0..555 {
        let mut pane = template.clone();
        pane.id = CardId::new();
        pane.title = format!("Pane {index}");
        pane.scope = Some(PathBuf::from(format!("/repo/scope-{index}")));
        if let CardKind::Browser(browser) = &mut pane.kind {
            browser.id = BrowserId::new();
        }
        let open = index < 122;
        durable.cards.push(CardState {
            id: pane.id,
            generation: 0,
            title: pane.title.clone(),
            width_fraction: pane.width_fraction,
            existence: if open {
                Existence::Open
            } else {
                super::super::project_ui::removed_existence("historical fixture")
            },
            kind: pane.kind.clone(),
        });
        durable
            .card_scopes
            .insert(pane.id, pane.scope.clone().unwrap());
        app.cards.insert(pane.id, CardRuntime::Completed);
        if open {
            let channel = index % 30;
            durable.channels[channel].cards.push(pane.id);
            durable.channels[channel].focused.get_or_insert(pane.id);
            model.views[channel].panes.push(pane);
        }
    }
    for index in 0..1218 {
        let scope = Scope::Worktree {
            repository: repository.clone(),
            path: format!("/repo/old-{index}").into(),
        };
        app.shared_views.initialized.insert(scope.clone());
        model.views.push(View::empty(scope));
    }
    assert_eq!(model.views.len(), 1248);
    assert_eq!(
        model
            .views
            .iter()
            .map(|view| view.panes.len())
            .sum::<usize>(),
        122
    );
    (app, model)
}

/// Every historical scope of `model` is a worktree the sidebar still
/// lists, as on a long-lived desktop.
fn list_historical_worktrees(app: &mut Cagents, model: &crate::workspace_model::WorkspaceModel) {
    for view in &model.views {
        if let Scope::Worktree { path, .. } = &view.scope {
            app.known_worktree_roots.insert(path.clone());
            app.sidebar_model.projects[0]
                .worktrees
                .push(crate::sidebar::SidebarWorktree::new(
                    crate::model::WorktreeId::new(),
                    path.clone(),
                    None,
                    false,
                    false,
                    None,
                    None,
                    None,
                ));
        }
    }
}

/// #2872: a complete save projects the scopes it can publish to, not every
/// historical one, and publishes what changed in them as before.
#[test]
fn a_complete_save_projects_only_the_scopes_it_can_publish() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, model) = desktop_inventory_fixture();
    list_historical_worktrees(&mut app, &model);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
    app.persist();
    std::iter::from_fn(|| rx.try_recv().ok()).for_each(drop);
    let channels = app
        .durable
        .channels
        .iter()
        .map(|channel| Scope::Channel(channel.id))
        .collect::<Vec<_>>();
    assert_eq!(
        app.local_views().scopes,
        channels,
        "1,218 settled historical worktrees have nothing to publish"
    );
    assert_eq!(
        app.local_view_scopes().len(),
        1_248,
        "all are still discovered"
    );

    // A reported title is published to the channel that shows it, alone.
    let titled = app.durable.channels[3].cards[0];
    app.card_state_mut(titled).unwrap().title = "Reported".into();
    app.persist();
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        matches!(edits.as_slice(), [ModelEdit::ChangeView {
            scope,
            change: Change::Update { pane, .. },
            ..
        }] if *scope == channels[3] && pane.id == titled && pane.title == "Reported"),
        "{edits:?}"
    );

    // A historical worktree that gains a card here is projected again.
    let historical = model.views[40].scope.clone();
    let Scope::Worktree { path, .. } = &historical else {
        panic!("a historical worktree")
    };
    let mut card = app.durable.cards.first().unwrap().clone();
    card.id = CardId::new();
    if let CardKind::Browser(browser) = &mut card.kind {
        browser.id = BrowserId::new();
    }
    let durable = Arc::make_mut(&mut app.durable);
    durable.card_scopes.insert(card.id, path.clone());
    durable.cards.push(card.clone());
    assert!(app.local_views().scopes.contains(&historical));
    app.persist();
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        matches!(edits.as_slice(), [ModelEdit::ChangeView {
            scope,
            change: Change::Insert { pane, .. },
            ..
        }] if *scope == historical && pane.id == card.id),
        "{edits:?}"
    );

    // Once its last card leaves, it still holds a published pane whose
    // removal is this scope's to diff, so it stays projected.
    let published = Arc::clone(&app.durable);
    let mut without = (*published).clone();
    without.cards.retain(|kept| kept.id != card.id);
    without.card_scopes.remove(&card.id);
    app.durable = Arc::new(without);
    assert!(
        app.shared_views.baseline[&historical]
            .iter()
            .any(|pane| pane.id == card.id)
    );
    assert!(app.local_views().scopes.contains(&historical));
    app.durable = published;

    // A discovered scope that was never seeded is seeded.
    let unseeded = model.views[41].scope.clone();
    app.shared_views.initialized.remove(&unseeded);
    app.shared_views.baseline.remove(&unseeded);
    app.persist();
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        matches!(edits.as_slice(), [ModelEdit::SeedView { scope, panes, .. }]
            if *scope == unseeded && panes.is_empty()),
        "{edits:?}"
    );
}

/// Paired measurement for #2641: the UI thread's CPU for a burst of 20
/// channel renames observed from the registry, at desktop inventory scale.
/// Run with `--ignored --nocapture` on the same host for both revisions.
#[test]
#[ignore = "manual paired measurement of observed metadata edits"]
fn measure_observed_channel_renames_at_desktop_inventory_scale() {
    crate::app::projection_oracle::disable();
    use super::super::workspace_model_sync::ModelUpdate;
    fn thread_cpu() -> Duration {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `value` is a valid, writable timespec.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) },
            0
        );
        Duration::new(value.tv_sec as u64, value.tv_nsec as u32)
    }
    let (mut app, model) = desktop_inventory_fixture();
    list_historical_worktrees(&mut app, &model);
    let (tx, _rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            model.clone(),
        ))),
    );
    let channel = app.durable.channels[0].id;
    let mut cpu = Vec::new();
    let mut wall = Vec::new();
    for index in 0..20 {
        let title = format!("Measured rename {index:02}");
        let sequence = app.workspace_model.sequence + 1;
        let (started, started_cpu) = (Instant::now(), thread_cpu());
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                sequence,
                edit: ModelEdit::RetitleChannel {
                    id: channel,
                    title: title.clone(),
                },
            })),
        );
        // The issue's burst is one rename per 100 ms. A deferred save
        // falls due once a second and costs what an immediate one did.
        if index % 10 == 9 && app.navigation_save.is_some() {
            drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        }
        cpu.push(thread_cpu() - started_cpu);
        wall.push(started.elapsed());
        assert_eq!(app.durable.channels[0].title, title);
    }
    let report = |name: &str, samples: &mut Vec<Duration>| {
        let total: Duration = samples.iter().sum();
        samples.sort();
        println!(
            "observed_renames {name}: total {:.3} ms, median {:.3} ms, max {:.3} ms",
            total.as_secs_f64() * 1e3,
            samples[samples.len() / 2].as_secs_f64() * 1e3,
            samples[samples.len() - 1].as_secs_f64() * 1e3,
        );
    };
    report("thread cpu", &mut cpu);
    report("wall", &mut wall);
}

/// Paired measurement for #2872: the UI thread's CPU for a complete save
/// at desktop inventory scale, and for the projection inside it.
#[test]
#[ignore = "manual paired measurement of complete saves"]
fn measure_complete_saves_at_desktop_inventory_scale() {
    crate::app::projection_oracle::disable();
    use super::super::workspace_model_sync::ModelUpdate;
    fn thread_cpu() -> Duration {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `value` is a valid, writable timespec.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) },
            0
        );
        Duration::new(value.tv_sec as u64, value.tv_nsec as u32)
    }
    let (mut app, model) = desktop_inventory_fixture();
    list_historical_worktrees(&mut app, &model);
    let (tx, _rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))));
    let measure = |name: &str, app: &mut Cagents, run: &dyn Fn(&mut Cagents)| {
        let mut samples = (0..20)
            .map(|_| {
                let started = thread_cpu();
                run(app);
                thread_cpu() - started
            })
            .collect::<Vec<_>>();
        samples.sort();
        println!(
            "complete_saves {name}: median {:.3} ms, max {:.3} ms",
            samples[samples.len() / 2].as_secs_f64() * 1e3,
            samples[samples.len() - 1].as_secs_f64() * 1e3,
        );
    };
    measure("discover", &mut app, &|app| {
        drop(app.discover_view_scopes().scopes);
    });
    measure("local_views", &mut app, &|app| drop(app.local_views()));
    measure("publish", &mut app, &|app| app.publish_view_changes());
    measure("persist", &mut app, &|app| app.persist());
}

fn observe_rename(app: &mut Cagents, channel: ChannelId, title: &str) {
    use super::super::workspace_model_sync::ModelUpdate;
    let sequence = app.workspace_model.sequence + 1;
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence,
        edit: ModelEdit::RetitleChannel {
            id: channel,
            title: title.into(),
        },
    }));
}

/// #2869: the save a rename burst defers hands the writer the burst's
/// last name, which a reopened store holds, and projects no view for it.
#[test]
#[cfg(not(feature = "integrated-browser"))]
fn a_deferred_rename_save_stores_the_latest_name_without_projecting_views() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, model) = desktop_inventory_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
    let directory = replica_capture::writer_fixture(&mut app);
    assert!(app.navigation_save.is_none());
    let channel = app.durable.channels[3].id;
    let offers = app.replica_capture.offers;
    for index in 0..5 {
        observe_rename(&mut app, channel, &format!("Renamed {index}"));
    }
    assert_eq!(app.replica_capture.offers, offers, "nothing is saved yet");
    std::iter::from_fn(|| rx.try_recv().ok()).for_each(drop);
    app.shared_views.local_projection_count.set(0);
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert_eq!(app.shared_views.local_projection_count.get(), 0);
    assert_eq!(
        app.replica_capture.offers,
        offers + 1,
        "one offer per burst"
    );
    assert!(app.navigation_save.is_none());
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::ChangeView { .. } | ModelEdit::SeedView { .. }
        ))
    );
    let mut writer = app.session.take().unwrap();
    writer.flush(Duration::from_secs(5)).unwrap();
    writer.shutdown();
    let (mut reopened, restored) = crate::session::SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("replica-capture"),
    )
    .unwrap();
    let restored = restored.unwrap();
    assert_eq!(
        restored
            .channels
            .iter()
            .find(|row| row.id == channel)
            .map(|row| row.title.as_str()),
        Some("Renamed 4"),
        "the latest name wins"
    );
    reopened.shutdown();
}

/// Navigation that joins a deferred rename save rides its timer, and the
/// save then publishes views as a navigation save does. A rename joining
/// pending navigation leaves it that way.
#[test]
fn navigation_joining_a_deferred_snapshot_save_publishes_views() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, model) = desktop_inventory_fixture();
    let (tx, _rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
    app.navigation_save = None;
    let channel = app.durable.channels[3].id;
    observe_rename(&mut app, channel, "First");
    let generation = app.navigation_save_generation;
    assert_eq!(app.navigation_save, Some(DeferredSave::Snapshot));
    drop(app.save_navigation_soon());
    observe_rename(&mut app, channel, "Second");
    assert_eq!(app.navigation_save, Some(DeferredSave::Views));
    assert_eq!(app.navigation_save_generation, generation, "one timer");
    app.shared_views.local_projection_count.set(0);
    drop(app.update(Message::NavigationSaveDue(generation)));
    assert!(app.shared_views.local_projection_count.get() > 0);
    assert!(app.navigation_save.is_none());
    // A timer from a burst already saved does nothing.
    observe_rename(&mut app, channel, "Third");
    app.shared_views.local_projection_count.set(0);
    let saved = super::super::terminal_output_tests::persists();
    drop(app.update(Message::NavigationSaveDue(generation)));
    assert_eq!(super::super::terminal_output_tests::persists(), saved);
    assert_eq!(app.navigation_save, Some(DeferredSave::Snapshot));
}

/// A deferred snapshot save still takes the strip into the active
/// channel, and a channel whose panes that changes publishes its view.
#[test]
fn a_deferred_snapshot_save_that_moves_a_pane_publishes_views() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, model) = desktop_inventory_fixture();
    let (tx, _rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
    let channel = app.durable.channels[3].id;
    drop(app.navigate_to_channel(channel));
    assert!(app.active_workspace.is_some());
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert!(app.navigation_save.is_none());
    observe_rename(&mut app, channel, "Renamed");
    assert_eq!(app.navigation_save, Some(DeferredSave::Snapshot));
    // A pane removed since the last save is still listed by its channel.
    let closed = CardId::new();
    Arc::make_mut(&mut app.durable).channels[3]
        .cards
        .push(closed);
    app.shared_views.local_projection_count.set(0);
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert!(!app.durable.channels[3].cards.contains(&closed));
    assert!(app.shared_views.local_projection_count.get() > 0);
    assert!(app.navigation_save.is_none());
}

/// #2641: a burst of renames from another window held the UI thread for
/// seconds, each one projecting every view the window holds and saving.
#[test]
fn observed_channel_renames_project_no_view_and_save_once_per_burst() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, model) = desktop_inventory_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
    app.navigation_save = None;
    let before = app.durable.clone();
    let channel = before.channels[3].id;
    let generation = app.navigation_save_generation;
    let persists = super::super::terminal_output_tests::persists;
    let saved = persists();
    let rename = |app: &mut Cagents, edit: ModelEdit| {
        app.shared_views.local_projection_count.set(0);
        let sequence = app.workspace_model.sequence + 1;
        drop(app.apply_workspace_model_update(ModelUpdate::Edited { sequence, edit }));
        assert_eq!(
            app.shared_views.local_projection_count.get(),
            0,
            "a name changes no view, so none is projected"
        );
        assert_eq!(app.workspace_model.sequence, sequence);
    };
    for index in 0..5 {
        let title = format!("Renamed {index}");
        rename(
            &mut app,
            ModelEdit::RetitleChannel {
                id: channel,
                title: title.clone(),
            },
        );
        assert_eq!(app.durable.channels[3].title, title);
        assert_eq!(
            app.sidebar_model
                .projects
                .iter()
                .flat_map(|project| &project.channels)
                .find(|row| row.id == channel)
                .map(|row| row.title.as_str()),
            Some(title.as_str()),
            "the sidebar shows the name"
        );
        assert!(app.navigation_save.is_some(), "the name is saved soon");
    }
    assert_eq!(
        app.navigation_save_generation,
        generation.wrapping_add(1),
        "the burst arms one save"
    );
    assert_eq!(persists(), saved, "no rename saves at once");
    assert_eq!(app.navigation_save, Some(DeferredSave::Snapshot));
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert_eq!(persists(), saved + 1, "the burst is saved once, after it");
    assert_eq!(
        app.shared_views.local_projection_count.get(),
        0,
        "#2869: nor does that save project a view"
    );
    assert!(app.navigation_save.is_none());
    // A suggestion cannot replace the manual name, so nothing changes and
    // nothing more is saved.
    app.navigation_save = None;
    rename(
        &mut app,
        ModelEdit::SuggestChannelTitle {
            id: channel,
            title: "Inferred".into(),
            source: crate::model::ChannelTitleSource::Provider,
        },
    );
    assert_eq!(app.durable.channels[3].title, "Renamed 4");
    assert!(app.navigation_save.is_none());
    // A conversation naming a channel that has no name yet is absorbed
    // the same way.
    let unnamed = before.channels[4].id;
    rename(
        &mut app,
        ModelEdit::RetitleChannel {
            id: unnamed,
            title: String::new(),
        },
    );
    rename(
        &mut app,
        ModelEdit::SuggestChannelTitle {
            id: unnamed,
            title: "Inferred".into(),
            source: crate::model::ChannelTitleSource::Provider,
        },
    );
    assert_eq!(app.durable.channels[4].title, "Inferred");
    assert_eq!(
        app.durable.channels[4].title_source,
        crate::model::ChannelTitleSource::Provider
    );
    assert!(app.navigation_save.is_some());
    assert_eq!(app.durable.cards, before.cards);
    assert_eq!(app.durable.card_scopes, before.card_scopes);
    for (after, before) in app.durable.channels.iter().zip(&before.channels) {
        assert_eq!(after.cards, before.cards);
        assert_eq!(after.focused, before.focused);
    }
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::ChangeView { .. } | ModelEdit::SeedView { .. }
        ))
    );
}

/// A name for a channel this window has not adopted still reaches the
/// full reconciliation, which adopts it.
#[test]
fn a_rename_of_a_channel_this_window_lacks_reconciles_in_full() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, mut model) = desktop_inventory_fixture();
    let (tx, _rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let unseen = ChannelId::new();
    model.channels.push(crate::workspace_model::ModelChannel {
        id: unseen,
        repository: PathBuf::from("/repo/.git"),
        title: "Elsewhere".into(),
        title_source: Default::default(),
        ordinal: 30,
        checkout: None,
    });
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
    Arc::make_mut(&mut app.durable)
        .channels
        .retain(|channel| channel.id != unseen);
    app.shared_views.local_projection_count.set(0);
    let sequence = app.workspace_model.sequence + 1;
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence,
        edit: ModelEdit::RetitleChannel {
            id: unseen,
            title: "Renamed elsewhere".into(),
        },
    }));
    assert!(app.shared_views.local_projection_count.get() > 0);
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == unseen)
            .map(|channel| channel.title.as_str()),
        Some("Renamed elsewhere")
    );
}

#[test]
fn complete_model_echo_reuses_one_projection_at_desktop_inventory_scale() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, mut model) = desktop_inventory_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let before = app.durable.clone();
    for _ in 0..8 {
        app.shared_views.local_projection_count.set(0);
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        assert_eq!(
            app.shared_views.local_projection_count.get(),
            1,
            "the completed baseline and publication share one real LocalViews pass"
        );
        assert_eq!(app.durable.cards, before.cards);
        assert_eq!(app.durable.channels, before.channels);
        assert_eq!(app.durable.card_scopes, before.card_scopes);
        assert_eq!(app.workspace.focused(), None);
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
                edit,
                ModelEdit::ChangeView { .. } | ModelEdit::SeedView { .. }
            ))
        );
    }
    // This is not an equal-model drop: actual canonical changes still
    // project whole pane fields, and a later equal Read rolls optimism back.
    let id = model.views[0].panes[0].id;
    model.sequence += 1;
    model.views[0].revision += 1;
    model.views[0].panes[0].title = "Remote title".into();
    model.views[0].panes[0].width_fraction = 0.72;
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
    assert_eq!(app.card_state(id).unwrap().title, "Remote title");
    assert_eq!(app.card_state(id).unwrap().width_fraction, 0.72);
    app.card_state_mut(id).unwrap().title = "Unaccepted optimism".into();
    drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
    assert_eq!(app.card_state(id).unwrap().title, "Remote title");
    assert_eq!(app.durable.cards.len(), 555);
}

/// Process-fresh synchronous reducer cost; excludes task execution, providers
/// and compositor presentation. Run each revision with --profile dev-runtime.
#[test]
#[ignore = "manual paired fw16 performance measurement"]
fn measure_desktop_startup_reconciliation() {
    crate::app::projection_oracle::disable();
    use super::super::workspace_model_sync::ModelUpdate;
    fn cpu_ns() -> u64 {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) },
            0
        );
        value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
    }
    let (mut app, model) = desktop_inventory_fixture();
    // Include the historical checkout inventory in the root/authority
    // indexes, even though its saved panes are closed.
    for view in &model.views {
        if let Scope::Worktree { path, .. } = &view.scope {
            app.known_worktree_roots.insert(path.clone());
            app.sidebar_model.projects[0]
                .worktrees
                .push(crate::sidebar::SidebarWorktree::new(
                    crate::model::WorktreeId::new(),
                    path.clone(),
                    None,
                    false,
                    false,
                    None,
                    None,
                    None,
                ));
        }
    }
    let (tx, _rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let wall = Instant::now();
    let cpu = cpu_ns();
    for _ in 0..8 {
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
    }
    println!(
        "desktop_reconciliation views={} cards={} panes={} reads=8 cpu_ns={} wall_ns={}",
        model.views.len(),
        app.durable.cards.len(),
        model
            .views
            .iter()
            .map(|view| view.panes.len())
            .sum::<usize>(),
        cpu_ns() - cpu,
        wall.elapsed().as_nanos()
    );
    let wall = Instant::now();
    let cpu = cpu_ns();
    for _ in 0..8 {
        drop(app.reconcile_shared_views_for_repository(Path::new("/repo/.git"), &HashSet::new()));
    }
    println!(
        "desktop_repository_reconciliation views={} cards={} roots={} passes=8 cpu_ns={} wall_ns={}",
        model.views.len(),
        app.durable.cards.len(),
        app.known_worktree_roots.len(),
        cpu_ns() - cpu,
        wall.elapsed().as_nanos()
    );
    // A restored workspace carries a registry identity; its readiness
    // checks must discover scopes instead of taking the fresh-window guard.
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(model.identity);
    let wall = Instant::now();
    let cpu = cpu_ns();
    for _ in 0..64 {
        assert!(std::hint::black_box(app.shared_views_startup_hold()).is_none());
    }
    println!(
        "desktop_startup_readiness roots={} passes=64 cpu_ns={} wall_ns={}",
        app.known_worktree_roots.len(),
        cpu_ns() - cpu,
        wall.elapsed().as_nanos()
    );
    assert_eq!(app.durable.cards.len(), 555);
    assert_eq!(app.workspace_model.views, model.views);
}

#[test]
fn an_exact_populated_view_does_not_need_a_migration_receipt() {
    let (mut app, source, _, pane) = channel_move_fixture();
    let scope = Scope::Channel(source);
    app.shared_views.initialized.remove(&scope);
    app.shared_views.baseline.remove(&scope);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.initialized.contains(&scope));
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(
            |edit| matches!(edit, ModelEdit::SeedView { scope: seeded, .. } if seeded == scope)
        )
    );
}

#[test]
fn many_matching_populated_views_publish_only_the_genuine_migration() {
    let (mut app, source, _, template) = channel_move_fixture();
    let channel_template = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == source)
        .unwrap()
        .clone();
    let model_template = app.workspace_model.channel(source).unwrap().clone();
    let mut migration = None;
    for index in 0..97 {
        let channel_id = ChannelId::new();
        let mut pane = template.clone();
        pane.id = CardId::new();
        pane.title = format!("matching pane {index}");
        if let CardKind::Browser(browser) = &mut pane.kind {
            browser.id = BrowserId::new();
        }
        let mut card = app.card_state(template.id).unwrap().clone();
        card.id = pane.id;
        card.title.clone_from(&pane.title);
        card.kind.clone_from(&pane.kind);
        let mut channel = channel_template.clone();
        channel.id = channel_id;
        channel.cards = vec![pane.id];
        channel.focused = Some(pane.id);
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(card);
        durable.channels.push(channel);
        app.cards.insert(pane.id, CardRuntime::Completed);
        let mut shared_channel = model_template.clone();
        shared_channel.id = channel_id;
        app.workspace_model.channels.push(shared_channel);
        let mut view = View::empty(channel_id);
        if index == 96 {
            migration = Some(view.scope.clone());
        } else {
            view.panes.push(pane);
        }
        app.workspace_model.views.push(view);
    }
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    let seeds = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|edit| {
            if let ModelEdit::SeedView { scope, .. } = edit {
                Some(scope)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(seeds, [migration.unwrap()]);
}

#[test]
fn exact_chat_admission_tracks_attachment_without_changing_source_or_draft() {
    let (mut app, source, _, pane) = retained_chat_move_fixture();
    let scope = Scope::Channel(source);
    app.shared_views.initialized.remove(&scope);
    app.shared_views.baseline.remove(&scope);
    app.shared_views.attached.remove(&pane.id);
    app.chat_ui = super::super::chat_ui::State::with_drafts(HashMap::from([(
        pane.id,
        "Saved unsent draft  ".into(),
    )]));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.initialized.contains(&scope));
    assert!(app.shared_views.attached.contains(&pane.id));
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&pane.id)
            .map(String::as_str),
        Some("Saved unsent draft  ")
    );
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(
            |edit| matches!(edit, ModelEdit::SeedView { scope: seeded, .. } if seeded == scope)
        )
    );
}

#[test]
fn scoped_admission_preserves_unrelated_active_channel_snapshot() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    app.active_workspace = Some("/repo".into());
    let mut next = pane.clone();
    next.id = CardId::new();
    if let CardKind::Browser(browser) = &mut next.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .panes
        .push(next.clone());
    drop(app.reconcile_shared_views());
    let saved = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == source)
        .unwrap()
        .clone();
    let _ = app.workspace.reorder(next.id, 0).unwrap();
    let _ = app.workspace.focus_without_scrolling(next.id);
    let selected = HashSet::from([Scope::Channel(destination)]);
    app.shared_views
        .initialized
        .remove(&Scope::Channel(destination));
    app.shared_views
        .baseline
        .remove(&Scope::Channel(destination));

    app.admit_existing_views_in(Some(&selected));

    assert!(
        app.shared_views
            .initialized
            .contains(&Scope::Channel(destination))
    );
    let current = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == source)
        .unwrap();
    assert_eq!(
        current.cards, saved.cards,
        "admission is a read-only eligibility check"
    );
    assert_eq!(current.focused, saved.focused);
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [next.id, pane.id]
    );
    assert_eq!(app.workspace.focused(), Some(next.id));
}

#[test]
fn admission_reads_mounted_order_without_reordering_unmounted_saved_members() {
    let (mut app, source, _, first) = channel_move_fixture();
    app.active_workspace = Some("/repo".into());
    let mut second = first.clone();
    second.id = CardId::new();
    if let CardKind::Browser(browser) = &mut second.kind {
        browser.id = BrowserId::new();
    }
    let scope = Scope::Channel(source);
    app.workspace_model.views[0].panes.push(second.clone());
    drop(app.reconcile_shared_views());
    let _ = app.workspace.focus_without_scrolling(second.id);
    app.workspace.request_close(first.id).unwrap();
    let _ = app.workspace.commit_close(first.id).unwrap();
    app.workspace_model.views[0].panes.swap(0, 1);
    app.shared_views.initialized.remove(&scope);
    app.shared_views.baseline.remove(&scope);

    app.admit_existing_views_in(Some(&HashSet::from([scope.clone()])));

    assert!(app.shared_views.initialized.contains(&scope));
    assert_eq!(app.durable.channels[0].cards, [first.id, second.id]);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [second.id]);
    assert_eq!(app.workspace.focused(), Some(second.id));
    assert!(app.card_state(first.id).unwrap().existence.is_open());
    assert_eq!(app.local_view_cards(&scope), [second.id, first.id]);

    drop(app.reconcile_shared_views());

    assert_eq!(app.durable.channels[0].cards, [second.id, first.id]);
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [second.id, first.id]
    );
    assert_eq!(app.workspace.focused(), Some(second.id));
}

#[test]
fn admission_ignores_closed_saved_members_without_mutating_the_saved_membership() {
    let (mut app, source, _, first) = channel_move_fixture();
    app.active_workspace = Some("/repo".into());
    let scope = Scope::Channel(source);
    let closed = CardId::new();
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(CardState {
        id: closed,
        generation: 0,
        title: "closed saved member".into(),
        width_fraction: 0.5,
        existence: super::super::project_ui::removed_existence("already closed"),
        kind: first.kind.clone(),
    });
    durable.channels[0].cards.push(closed);
    app.shared_views.initialized.remove(&scope);
    app.shared_views.baseline.remove(&scope);

    app.admit_existing_views_in(Some(&HashSet::from([scope.clone()])));

    assert!(app.shared_views.initialized.contains(&scope));
    assert_eq!(app.durable.channels[0].cards, [first.id, closed]);
    assert_eq!(
        view_scope_hold(&app.shared_views, std::iter::once(&scope)),
        None
    );
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(
            |edit| matches!(edit, ModelEdit::SeedView { scope: seeded, .. } if seeded == scope)
        )
    );
    assert_eq!(app.durable.channels[0].cards, [first.id]);
    assert!(!app.card_state(closed).unwrap().existence.is_open());
}

#[test]
fn populated_admission_checks_the_current_strip_order_before_equality() {
    let (mut app, source, _, pane) = channel_move_fixture();
    app.active_workspace = Some("/repo".into());
    let mut next = pane.clone();
    next.id = CardId::new();
    if let CardKind::Browser(browser) = &mut next.kind {
        browser.id = BrowserId::new();
    }
    let scope = Scope::Channel(source);
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap()
        .panes
        .push(next.clone());
    drop(app.reconcile_shared_views());
    app.shared_views.initialized.remove(&scope);
    app.shared_views.baseline.remove(&scope);
    let _ = app.workspace.reorder(next.id, 0).unwrap();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    assert!(!app.shared_views.initialized.contains(&scope));
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [next.id, pane.id]
    );
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|edit|
        matches!(edit, ModelEdit::SeedView { scope: seeded, panes, .. }
            if seeded == scope && panes.iter().map(|pane| pane.id).collect::<Vec<_>>() == [next.id, pane.id])));
}

#[test]
fn populated_admission_does_not_substitute_logical_source_equality() {
    let (mut app, source, _, pane) = channel_move_fixture();
    let scope = Scope::Channel(source);
    app.shared_views.initialized.remove(&scope);
    app.shared_views.baseline.remove(&scope);
    let CardKind::Browser(browser) = &mut app.card_state_mut(pane.id).unwrap().kind else {
        panic!("browser fixture")
    };
    browser.current_url = "https://example.test/changed".parse().unwrap();
    app.admit_existing_views_without_migration();
    assert!(!app.shared_views.initialized.contains(&scope));
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.shared_views.retained.insert(pane.id);
    app.admit_existing_views_without_migration();
    assert!(!app.shared_views.initialized.contains(&scope));
}

#[test]
fn authoritative_empty_views_do_not_round_trip_an_empty_migration() {
    let (mut app, existing, absent, _) = fixture();
    app.shared_views.initialized.clear();
    app.workspace_model.views = vec![View::empty(existing)];
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !edits.iter().any(|edit| matches!(edit,
        ModelEdit::SeedView { scope: Scope::Channel(id), .. } if *id == existing)),
        "the host already holds this empty view; no migration or receipt is needed"
    );
    assert!(
        app.shared_views
            .initialized
            .contains(&Scope::Channel(existing))
    );
    assert!(
        edits.iter().any(|edit| matches!(edit,
        ModelEdit::SeedView { scope: Scope::Channel(id), .. } if *id == absent)),
        "an absent view still needs the host to create its authority"
    );
}

#[test]
fn empty_view_admission_keeps_local_members_and_pending_intentions_behind_the_seed() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let source = Scope::Channel(source);
    let destination = Scope::Channel(destination);
    app.shared_views.initialized.clear();
    app.workspace_model
        .views
        .iter_mut()
        .for_each(|view| view.panes.clear());
    let operation = uuid::Uuid::new_v4();
    app.shared_views.pending.push((
        operation,
        destination.clone(),
        Change::Remove { id: CardId::new() },
    ));
    app.admit_existing_views_without_migration();
    assert!(
        !app.shared_views.initialized.contains(&source),
        "local member still needs migration"
    );
    assert!(
        !app.shared_views.initialized.contains(&destination),
        "pending intention still needs its receipt"
    );
    assert_eq!(app.local_view_cards(&source), vec![pane.id]);
    app.shared_views.pending.clear();
    app.shared_views.seeding.insert(destination.clone());
    app.admit_existing_views_without_migration();
    assert!(
        !app.shared_views.initialized.contains(&destination),
        "an in-flight seed still owns its barrier"
    );
}

#[test]
fn an_unprojected_worktree_default_remains_a_local_migration_intention() {
    let (mut app, _, _, pane) = channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let repository = PathBuf::from("/repo/.git");
    let path = PathBuf::from("/repo/default-worktree");
    let scope = Scope::Worktree {
        repository: repository.clone(),
        path: path.clone(),
    };
    app.sidebar_model.projects.push(SidebarProject::new(
        ProjectId::new(),
        RepositoryId::new(),
        "repo".into(),
        repository,
        vec![],
        vec![],
    ));
    app.known_worktree_roots.insert(path.clone());
    let durable = Arc::make_mut(&mut app.durable);
    for channel in &mut durable.channels {
        channel.cards.retain(|id| *id != pane.id);
    }
    durable.card_scopes.insert(pane.id, path);
    let CardKind::Terminal(session) = pane.kind else {
        panic!("terminal fixture")
    };
    app.shared_views.defaults.insert(pane.id, session);
    app.workspace_model.views.push(View::empty(scope.clone()));
    assert_eq!(app.local_view_cards(&scope), [pane.id]);
    assert!(
        app.local_view_panes(&scope).is_empty(),
        "default is withheld from pane publication"
    );
    app.admit_existing_views_without_migration();
    assert!(
        !app.shared_views.initialized.contains(&scope),
        "membership preserves the default's seed barrier"
    );
}

#[test]
fn empty_view_admission_requires_registry_authority() {
    let (mut app, existing, _, _) = fixture();
    let scope = Scope::Channel(existing);
    app.shared_views.initialized.clear();
    app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
    app.workspace_model.views = vec![View::empty(scope.clone())];
    app.admit_existing_views_without_migration();
    assert!(!app.shared_views.initialized.contains(&scope));
}

#[test]
fn seed_ack_projects_only_its_scope_and_preserves_unrelated_pending_edits() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let source = Scope::Channel(source);
    let destination = Scope::Channel(destination);
    let unrelated = uuid::Uuid::new_v4();
    let change = Change::Remove { id: CardId::new() };
    app.shared_views
        .pending
        .push((unrelated, destination.clone(), change.clone()));
    app.shared_views.seeding.insert(source.clone());
    app.shared_views.initialized.remove(&source);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    app.shared_views.local_projection_count.set(0);
    app.acknowledge_view_edit(source.clone(), None);
    assert_eq!(
        app.shared_views.local_projection_count.get(),
        0,
        "a seed receipt must not reconstruct unrelated scopes"
    );
    assert!(app.shared_views.initialized.contains(&source));
    assert!(!app.shared_views.seeding.contains(&source));
    assert!(
        app.shared_views
            .pending
            .contains(&(unrelated, destination.clone(), change))
    );
    assert!(
        app.shared_views.baseline[&source]
            .iter()
            .any(|item| item.id == pane.id)
    );
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
        ModelEdit::ChangeView { scope, .. } | ModelEdit::SeedView { scope, .. }
        if scope == destination))
    );
}

#[test]
fn reused_projection_seeds_new_scopes_and_drains_edits_after_equal_ack() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, source, destination, pane) = channel_move_fixture();
    let mut next = pane.clone();
    next.id = CardId::new();
    if let CardKind::Browser(browser) = &mut next.kind {
        browser.id = BrowserId::new();
    }
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(CardState {
        id: next.id,
        generation: 0,
        title: next.title.clone(),
        width_fraction: next.width_fraction,
        existence: Existence::Open,
        kind: next.kind.clone(),
    });
    durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == destination)
        .unwrap()
        .cards = vec![next.id];
    app.cards.insert(next.id, CardRuntime::Completed);
    app.shared_views
        .initialized
        .remove(&Scope::Channel(destination));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    let requests = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(requests.iter().any(|edit| matches!(edit,
        ModelEdit::SeedView { scope, panes, .. } if scope == &Scope::Channel(destination) && panes == &vec![next.clone()])));
    app.card_state_mut(next.id).unwrap().title = "While seed is pending".into();
    app.persist();
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|edit| matches!(edit, ModelEdit::ChangeView { .. }))
    );
    let mut model = app.workspace_model.clone();
    model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap()
        .panes = vec![next.clone()];
    // A seed acknowledgement remains semantically required even with no
    // newer outer sequence: it initializes and drains the retained edit.
    drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(destination),
        operation: None,
        model,
    }));
    let requests = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(requests.iter().any(|edit| matches!(edit,
        ModelEdit::ChangeView { scope, change: Change::Update { pane, .. }, .. }
        if scope == &Scope::Channel(destination) && pane.id == next.id && pane.title == "While seed is pending")));
    assert_eq!(
        app.card_state(next.id).unwrap().title,
        "While seed is pending"
    );
    // Normal local persistence still snapshots an active strip and emits
    // its changed width; reuse is confined to completed reconciliation.
    let _ = app
        .workspace
        .set_card_width(pane.id, CardWidth::Fraction(0.68))
        .unwrap();
    app.card_state_mut(pane.id).unwrap().width_fraction = 0.68;
    app.persist();
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
        ModelEdit::ChangeView { scope, change: Change::Update { pane: changed, .. }, .. }
        if scope == Scope::Channel(source) && changed.id == pane.id && changed.width_fraction == 0.68)));
}

/// Settled metadata and a ready browser are ordinary presentation state;
/// history and unrelated default drafts are deliberately not required.
#[cfg(not(feature = "integrated-browser"))]
fn presentation_fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let repository = PathBuf::from("/repo/.git");
    let project = app.durable.channels[0].project_id;
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        repository.clone(),
        vec![],
        vec![],
    ));
    app.workspace_model
        .projects
        .push(crate::workspace_model::ModelProject {
            repository,
            name: "repo".into(),
            kind: ProjectKind::Git,
            ordinal: 0,
        });
    drop(app.reconcile_workspace_model());
    let view = app
        .workspace_model
        .views
        .iter()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap();
    let mut next = view.clone();
    next.revision += 1;
    assert_eq!(app.presentation_projection_ready(view, &next, None), Ok(()));
    (app, source, destination, pane)
}

#[cfg(not(feature = "integrated-browser"))]
fn presentation_managed_notice(
    app: &mut Cagents,
    channel: ChannelId,
    card: CardId,
) -> crate::notifications::Notice {
    use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
    let CardKind::Terminal(session) = moving_terminal_kind() else {
        unreachable!()
    };
    let repository = app
        .workspace_model
        .channel(channel)
        .unwrap()
        .repository
        .clone();
    let state = CardState {
        id: card,
        generation: 0,
        title: "Managed attention".into(),
        width_fraction: 0.5,
        existence: Existence::Open,
        kind: CardKind::Terminal(session.clone()),
    };
    let durable = Arc::make_mut(&mut app.durable);
    if let Some(existing) = durable.cards.iter_mut().find(|row| row.id == card) {
        *existing = state.into();
    } else {
        durable.cards.push(state);
    }
    durable.card_scopes.remove(&card);
    for row in &mut durable.channels {
        row.cards.retain(|id| *id != card);
        if row.id == channel {
            row.cards.push(card);
        }
    }
    let pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
    for view in &mut app.workspace_model.views {
        view.panes.retain(|pane| pane.id != card);
    }
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(channel))
        .unwrap()
        .panes
        .push(pane);
    app.workspace_model.agents.push(Agent {
        id: crate::model::AgentId::from_uuid(card.as_uuid()),
        name: None,
        origin: Origin::User,
        spec: Spec {
            provider: crate::agent::Provider::Claude,
            account: None,
            target: Target {
                repository,
                directory: session.launch.working_directory.clone(),
                placement: Some(Scope::Channel(channel)),
            },
            surface: Surface::Terminal,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement: Scope::Channel(channel),
        session: Some(HostedSession::Terminal(session.clone())),
        console: Some(session.clone()),
        created_ms: 0,
        closed: false,
    });
    app.cards.insert(card, CardRuntime::Loading);
    app.register_presence(card, Exposure::Offscreen, Residency::Cold);
    app.shared_views.attached.insert(card);
    crate::notifications::Notice {
        id: uuid::Uuid::new_v4(),
        delivered: false,
        acknowledged: false,
        window: None,
        event: crate::notifications::Event {
            content: None,
            target: crate::notifications::Target::Session(session.id),
            source_session: Some(session.id),
            provider: Some(crate::agent::Provider::Claude),
            directory: Some(session.launch.working_directory),
            destination: None,
            evidence: crate::notifications::Evidence {
                key: format!("presentation-managed-{card}"),
                kind: crate::notifications::Kind::Failed,
            },
        },
    }
}

#[cfg(not(feature = "integrated-browser"))]
fn presentation_edge(
    app: &Cagents,
    scope: Scope,
    revision: u64,
    title: &str,
    width: f32,
) -> super::super::workspace_model_sync::ModelUpdate {
    use super::super::workspace_model_sync::{EdgeChanges, ModelUpdate};
    use crate::workspace_model::delta::{EditEffects, ViewChanges, ViewRevision};
    let mut view = app
        .workspace_model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .clone();
    let before = view.panes[0].clone();
    view.panes[0].title = title.into();
    view.panes[0].width_fraction = width;
    let change = Change::Update {
        before,
        pane: view.panes[0].clone(),
    };
    let expected_revision = view.revision;
    view.revision = revision;
    ModelUpdate::Changed {
        sequence: revision,
        edit: ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision,
            operation: uuid::Uuid::new_v4(),
            change,
        },
        changes: EdgeChanges {
            identity: Some(app.workspace_model.identity),
            views: ViewChanges {
                sequence: revision,
                views: vec![view],
                ..Default::default()
            },
            effects: EditEffects {
                views: vec![ViewRevision {
                    scope,
                    before: Some(expected_revision),
                    after: Some(revision),
                }],
                ..Default::default()
            },
        },
    }
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_edge_uses_the_selected_projector_and_chrome_update_tail() {
    let (mut app, source, destination, pane) = presentation_fixture();
    // Match the native bounded workload's saved history. Only 83 panes
    // are open; historical roots/cards must not enter this edit's local
    // discovery or projection budget.
    for index in 1..1_769 {
        let mut saved = app.card_state(pane.id).unwrap().clone();
        saved.id = CardId::new();
        if let CardKind::Browser(browser) = &mut saved.kind {
            browser.id = BrowserId::new();
        }
        if index == 1 {
            saved.kind = moving_terminal_kind();
        }
        if index < 83 {
            let path = PathBuf::from(format!("/repo/open-{index}"));
            let scope = Scope::Worktree {
                repository: PathBuf::from("/repo/.git"),
                path: path.clone(),
            };
            app.workspace_model.views.push(View {
                scope,
                revision: 1,
                panes: vec![Pane::from_card(&saved, Some(path.clone())).unwrap()],
                retired: vec![],
            });
            Arc::make_mut(&mut app.durable)
                .card_scopes
                .insert(saved.id, path);
            if index == 1 {
                app.cards.insert(saved.id, CardRuntime::Loading);
                app.shared_views.attached.insert(saved.id);
            } else {
                app.cards.insert(saved.id, CardRuntime::Completed);
            }
        } else {
            saved.existence = super::super::project_ui::removed_existence("historical fixture");
        }
        let id = saved.id;
        Arc::make_mut(&mut app.durable).cards.push(saved);
        if index == 1 {
            app.register_presence(id, Exposure::Offscreen, Residency::Cold);
        }
    }
    while app.workspace_model.views.len() < 1_856 {
        app.workspace_model.views.push(View::empty(Scope::Worktree {
            repository: PathBuf::from("/repo/.git"),
            path: PathBuf::from(format!("/repo/closed-{}", app.workspace_model.views.len())),
        }));
    }
    let managed = app
        .durable
        .cards
        .iter()
        .skip(1)
        .take(20)
        .map(|card| card.id)
        .collect::<Vec<_>>();
    let managed_channel = destination;
    let notices = managed
        .iter()
        .map(|card| presentation_managed_notice(&mut app, managed_channel, *card))
        .collect::<Vec<_>>();
    drop(app.update_notifications(notification_ui::Action::Snapshot(
        crate::notifications::Snapshot {
            notices,
            diagnostic: None,
        },
    )));
    assert!(
        managed
            .iter()
            .all(|card| app.sidebar_state.has_card_update(*card))
    );
    let attention = managed
        .iter()
        .map(|card| app.sidebar_state.card_attention(*card))
        .collect::<Vec<_>>();
    // A settled window has already initialized historical chrome and
    // terminal ordinals. Only selected members should be visited again.
    drop(app.sync_chrome());
    app.shared_views.chrome_cards_visited.set(0);
    app.shared_views.publisher_index_entries = 0;
    app.shared_views.publisher_scope_comparisons = 0;
    app.shared_views.order_rewrites = 0;
    app.shared_views.held_descriptor_clones.set(0);
    app.shared_views.notification_attribution_count.set(0);
    let failure = match app.cards.get(&pane.id) {
        Some(CardRuntime::Failed(error)) => format!("{error:?}"),
        _ => panic!("browserless unavailable surface"),
    };
    let residency = app.residency(pane.id).clone();
    let exposure = app.exposure(pane.id);
    app.shared_views.pane_attachment_count = 0;
    let listed_passive = app.durable.cards[1].id;
    let history = app
        .durable
        .cards
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>();
    let views = app.workspace_model.views[2..].to_vec();
    let update = presentation_edge(&app, Scope::Channel(source), 21, "Remote label", 0.72);
    app.shared_views.scope_discovery_count.set(0);
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    app.shared_views.structural_tail_count = 0;
    app.shared_views.chrome_tail_count = 0;
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(
        app.shared_views.scope_discovery_count.get(),
        0,
        "settled presentation does not rediscover historical scopes in a release tail"
    );
    assert_eq!(
        app.shared_views.notification_attribution_count.get(),
        0,
        "stable presentation does not rebuild managed notification ownership"
    );
    assert_eq!(
        managed
            .iter()
            .map(|card| app.sidebar_state.card_attention(*card))
            .collect::<Vec<_>>(),
        attention
    );
    assert_eq!(app.shared_views.local_projection_count.get(), 0);
    assert_eq!(app.shared_views.selected_projection_count.get(), 1);
    assert_eq!(app.shared_views.structural_tail_count, 0);
    assert_eq!(app.shared_views.chrome_tail_count, 1);
    assert_eq!(app.shared_views.chrome_cards_visited.get(), 1);
    assert_eq!(app.shared_views.publisher_index_entries, 1);
    assert_eq!(app.shared_views.publisher_scope_comparisons, 1);
    assert_eq!(app.shared_views.order_rewrites, 0);
    assert_eq!(app.shared_views.held_descriptor_clones.get(), 0);
    assert_eq!(app.card_state(pane.id).unwrap().title, "Remote label");
    assert_eq!(app.card_state(pane.id).unwrap().width_fraction, 0.72);
    assert!(
        rx.try_recv().is_err(),
        "a remote presentation is not republished"
    );
    assert!(app.durable.cards.iter().skip(1).eq(history.iter()));
    assert_eq!(&app.workspace_model.views[2..], views.as_slice());
    assert!(
        matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(error)) if format!("{error:?}") == failure)
    );
    assert_eq!(app.residency(pane.id), &residency);
    assert_eq!(app.exposure(pane.id), exposure);
    assert_eq!(app.shared_views.pane_attachment_count, 0);
    assert!(!app.terminal_start_in_flight(pane.id));
    assert!(matches!(
        app.cards.get(&listed_passive),
        Some(CardRuntime::Loading)
    ));
    assert_eq!(app.residency(listed_passive), &Residency::Cold);
    assert!(app.shared_views.attached.contains(&listed_passive));
    assert!(!app.saved_terminals.entries.contains_key(&listed_passive));
    assert!(!app.terminal_start_in_flight(listed_passive));
}

#[test]
fn presentation_selected_publication_finds_an_unprojected_transfer_source() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let source = Scope::Channel(source);
    let destination = Scope::Channel(destination);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    // Preserve first canonical-view semantics in the scoped lookup too.
    let mut duplicate = app.workspace_model.views[0].clone();
    duplicate.revision = 999;
    app.workspace_model.views.push(duplicate);
    for index in 0..100 {
        app.workspace_model.views.push(View::empty(Scope::Worktree {
            repository: PathBuf::from("/repo/.git"),
            path: PathBuf::from(format!("/repo/history-{index}")),
        }));
    }
    let baseline = app.shared_views.baseline.clone();
    let project = || LocalViews {
        scopes: vec![destination.clone()],
        cards: HashMap::from([(destination.clone(), vec![pane.id])]),
        panes: HashMap::from([(destination.clone(), vec![pane.clone()])]),
        settled: HashSet::new(),
    };
    app.shared_views.publisher_index_entries = 0;
    app.shared_views.publisher_scope_comparisons = 0;
    app.publish_projected_view_changes(
        project(),
        ViewProjection::Selected(std::slice::from_ref(&destination)),
    );
    assert_eq!(app.shared_views.publisher_index_entries, 2);
    assert_eq!(app.shared_views.publisher_scope_comparisons, 3);
    let scoped = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(scoped.len(), 1);
    let ModelEdit::ChangeView {
        scope,
        expected_revision,
        change,
        ..
    } = &scoped[0]
    else {
        panic!("transfer")
    };
    assert_eq!(scope, &source);
    assert_eq!(*expected_revision, 17);
    assert!(
        matches!(change, Change::Transfer { id, destination: to, .. } if *id == pane.id && to == &destination)
    );
    let scoped_baseline = app.shared_views.baseline.clone();
    app.shared_views.baseline = baseline.clone();
    app.shared_views.pending.clear();
    app.publish_projected_view_changes(project(), ViewProjection::All);
    let full = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(full.len(), scoped.len());
    let ModelEdit::ChangeView {
        scope: full_scope,
        expected_revision: full_revision,
        change: full_change,
        ..
    } = &full[0]
    else {
        panic!("transfer")
    };
    assert_eq!(
        (scope, expected_revision, change),
        (full_scope, full_revision, full_change)
    );
    assert_eq!(app.shared_views.baseline, scoped_baseline);

    app.shared_views.baseline = baseline;
    app.shared_views.pending.clear();
    app.shared_views.publisher_index_entries = 0;
    let builds = app.shared_views.view_index.builds;
    app.publish_projected_view_changes(
        project(),
        ViewProjection::Structural(&HashSet::from([destination.clone()])),
    );
    assert_eq!(
        app.shared_views.view_index.builds, builds,
        "the complete pass's index serves the same model's views"
    );
    assert_eq!(
        app.shared_views.publisher_index_entries, 2,
        "only the two required scopes are looked up"
    );
    let structural = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(structural.len(), scoped.len());
    assert!(matches!(&structural[0], ModelEdit::ChangeView {
        scope: structural_scope, expected_revision: structural_revision,
        change: structural_change, ..
    } if (structural_scope, structural_revision, structural_change) == (scope, expected_revision, change)));
    assert_eq!(app.shared_views.baseline, scoped_baseline);
}

#[test]
fn presentation_order_normalization_checks_durable_order_and_keeps_history_in_place() {
    let (mut app, _, _, pane) = channel_move_fixture();
    let mut other = app.card_state(pane.id).unwrap().clone();
    other.id = CardId::new();
    let mut history = other.clone();
    history.id = CardId::new();
    history.existence = super::super::project_ui::removed_existence("history");
    let first = app.card_state(pane.id).unwrap().clone();
    Arc::make_mut(&mut app.durable).cards =
        vec![first.clone(), history.clone(), other.clone()].into();
    app.index_cards();
    // COW leaves the old index cached; each lookup must validate its ID
    // before using that index as durable ordering evidence.
    Arc::make_mut(&mut app.durable).cards.swap(0, 2);
    // Channel order is already right; the durable selected subsequence is
    // deliberately reversed around an unrelated closed historical card.
    Arc::make_mut(&mut app.durable).channels[0].cards = vec![first.id, other.id];
    let before = app.durable.cards.clone();
    app.normalize_projected_card_order(&[first.id, other.id]);
    assert!(
        app.durable.cards.copied_from(&before).is_empty(),
        "reordering shares the original cards rather than copying their descriptors"
    );
    assert_eq!(app.durable.cards, vec![first, history, other].into());
    assert_eq!(app.shared_views.order_rewrites, 1);
    let held = app.durable.clone();
    app.normalize_projected_card_order(&[pane.id, held.cards[2].id]);
    assert!(
        Arc::ptr_eq(&held, &app.durable),
        "settled order never writes or clones history"
    );
    assert_eq!(app.shared_views.order_rewrites, 1);
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_scoped_chrome_matches_full_and_initializes_missing_caches() {
    let (mut app, source, _, pane) = presentation_fixture();
    let mut history = app.card_state(pane.id).unwrap().clone();
    history.id = CardId::new();
    history.existence = super::super::project_ui::removed_existence("history");
    Arc::make_mut(&mut app.durable).cards.push(history.clone());
    drop(app.sync_chrome());
    app.card_state_mut(pane.id).unwrap().title = "Canonical updated label".into();
    app.chrome_ui
        .user_titles
        .insert(pane.id, "My explicit label".into());
    app.shared_views.chrome_cards_visited.set(0);
    let members = app.presentation_chrome_cards(&Scope::Channel(source));
    assert_eq!(members, vec![pane.id]);
    drop(app.sync_scoped_chrome(&members));
    assert_eq!(app.shared_views.chrome_cards_visited.get(), 1);
    let chrome = &app.card_chrome[&pane.id];
    let observed = (
        chrome.title.clone(),
        chrome.caption.clone(),
        chrome.tooltip.clone(),
        chrome.focused_name.clone(),
        chrome.title_width,
    );
    let header = app.core.window.header_title.clone();
    let accessible = app.workspace.card(pane.id).unwrap().accessible_name.clone();
    drop(app.sync_chrome());
    let chrome = &app.card_chrome[&pane.id];
    assert_eq!(
        observed,
        (
            chrome.title.clone(),
            chrome.caption.clone(),
            chrome.tooltip.clone(),
            chrome.focused_name.clone(),
            chrome.title_width
        )
    );
    assert_eq!(app.core.window.header_title, header);
    assert_eq!(
        app.workspace.card(pane.id).unwrap().accessible_name,
        accessible
    );
    assert_eq!(accessible, "My explicit label");
    app.card_chrome.remove(&pane.id);
    app.shared_views.chrome_cards_visited.set(0);
    drop(app.sync_scoped_chrome(&members));
    // Missing chrome falls back to the full pass, which visits every card
    // that can be drawn and leaves the closed one without chrome.
    assert_eq!(
        app.shared_views.chrome_cards_visited.get(),
        app.durable.cards.len() - 1
    );
    assert!(app.card_chrome.contains_key(&pane.id));
    assert!(!app.card_chrome.contains_key(&history.id));
}

#[test]
fn presentation_missing_terminal_ordinal_uses_the_global_counter() {
    let (mut app, _, _, pane) = channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let mut history = app.card_state(pane.id).unwrap().clone();
    history.id = CardId::new();
    history.existence = super::super::project_ui::removed_existence("history");
    Arc::make_mut(&mut app.durable).cards.push(history.clone());
    drop(app.sync_chrome());
    let existing = app.chrome_ui.terminal_ordinals[&history.id];
    app.chrome_ui.terminal_ordinals.remove(&pane.id);
    app.shared_views.chrome_cards_visited.set(0);
    drop(app.sync_scoped_chrome(&[pane.id]));
    assert_eq!(
        app.shared_views.chrome_cards_visited.get(),
        app.durable.cards.len() - 1
    );
    // The closed terminal has no chrome but keeps its number.
    assert!(!app.card_chrome.contains_key(&history.id));
    assert!(app.chrome_ui.terminal_ordinals[&pane.id] > existing);
    assert_eq!(app.chrome_ui.terminal_ordinals[&history.id], existing);
    let chrome = &app.card_chrome[&pane.id];
    assert!(chrome.compact_title.is_some());
    assert!(chrome.terminal_title_source.is_some());
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_noop_projection_does_not_copy_a_writer_held_workspace() {
    let (mut app, source, destination, _) = presentation_fixture();
    // Exercise the real shared reducer on an inactive, settled scope,
    // where an unchanged receipt has no active-strip snapshot to save.
    Arc::make_mut(&mut app.durable).active_channel = Some(destination);
    let view = app
        .workspace_model
        .views
        .iter()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .clone();
    let current = app.local_view_cards(&view.scope);
    let owned = view.panes.iter().map(|pane| pane.id).collect();
    let writer = app.durable.clone();
    app.shared_views.applying = true;
    drop(app.project_shared_view_with_cards(&view, &owned, current));
    app.shared_views.applying = false;
    assert!(Arc::ptr_eq(&writer, &app.durable));
    assert_eq!(app.shared_views.order_rewrites, 0);
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_settings_blur_keeps_the_complete_common_tail() {
    let (mut app, source, _, _) = presentation_fixture();
    let _directory = replica_capture::writer_fixture(&mut app);
    let earlier = presentation_edge(&app, Scope::Channel(source), 21, "Earlier replica", 0.59);
    drop(app.update(Message::WorkspaceModelObserved(earlier)));
    assert!(app.replica_capture.pending.is_some());
    let captures = app.replica_capture.captures;
    drop(app.sync_chrome());
    let update = presentation_edge(&app, Scope::Channel(source), 24, "After blur", 0.61);
    // Focus has already left Settings, while its debounced draft still
    // belongs to the common post-dispatch blur processing.
    drop(app.queue_setting_input(SettingsField::TerminalFontSize, "18".into()));
    app.settings_pane_focused = true;
    app.shared_views.structural_tail_count = 0;
    app.shared_views.selected_projection_count.set(0);
    app.shared_views.local_projection_count.set(0);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.settings.terminal_font_size, 18.0);
    assert!(app.replica_capture.pending.is_none());
    assert!(app.replica_capture.captures > captures);
    assert!(app.settings_pending.is_empty());
    assert_eq!(app.shared_views.selected_projection_count.get(), 0);
    assert_eq!(app.shared_views.local_projection_count.get(), 1);
    assert_eq!(
        app.shared_views.structural_tail_count, 1,
        "a committed settings draft invalidates the scoped chrome proof"
    );
    drop(app);
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_scoped_chrome_includes_retained_utility_and_keeps_focus() {
    let (mut app, source, _, pane) = presentation_fixture();
    drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
    let utility = app.workspace.focused().unwrap();
    assert_eq!(app.card_state(utility).unwrap().kind, CardKind::Settings);
    // Opening Settings is saved with the navigation burst, and the scoped
    // projection waits for that save.
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert!(app.navigation_save.is_none());
    let utility_state = app.card_state(utility).unwrap().clone();
    assert!(
        app.presentation_chrome_cards(&Scope::Channel(source))
            .contains(&utility)
    );
    drop(app.sync_chrome());
    app.shared_views.chrome_cards_visited.set(0);
    app.shared_views.selected_projection_count.set(0);
    let update = presentation_edge(&app, Scope::Channel(source), 24, "Beside settings", 0.61);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.selected_projection_count.get(), 1);
    assert_eq!(app.shared_views.chrome_cards_visited.get(), 2);
    assert_eq!(app.workspace.focused(), Some(utility));
    assert_eq!(app.card_state(utility), Some(&utility_state));
    assert_eq!(app.card_state(pane.id).unwrap().title, "Beside settings");
    assert_eq!(app.card_chrome[&pane.id].title, browser_host::TITLE);
    assert_eq!(app.card_chrome[&utility].title, "Settings");
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_name_restoration_in_another_channel_refreshes_all_chrome() {
    let (mut app, source, destination, pane) = presentation_fixture();
    let mut other = app.card_state(pane.id).unwrap().clone();
    other.id = CardId::new();
    if let CardKind::Browser(browser) = &mut other.kind {
        browser.id = BrowserId::new();
    }
    let other_pane = Pane::from_card(&other, None).unwrap();
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(other.clone());
    let channel = durable
        .channels
        .iter_mut()
        .find(|row| row.id == destination)
        .unwrap();
    channel.cards.push(other.id);
    channel.title.clear();
    app.workspace_model
        .channels
        .iter_mut()
        .find(|row| row.id == destination)
        .unwrap()
        .title
        .clear();
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap()
        .panes
        .push(other_pane.clone());
    app.shared_views
        .initialize(Scope::Channel(destination), vec![other_pane]);
    app.cards.insert(other.id, CardRuntime::Completed);
    let _directory = replica_capture::writer_fixture(&mut app);
    let captures = app.replica_capture.captures;
    app.remember_channel_name_for_test(destination, "Retained channel name".into());
    drop(app.sync_chrome());
    app.shared_views.chrome_cards_visited.set(0);
    app.shared_views.selected_projection_count.set(0);
    let update = presentation_edge(&app, Scope::Channel(source), 24, "Independent title", 0.61);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.selected_projection_count.get(), 1);
    assert!(app.replica_capture.pending.is_none());
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|row| row.id == destination)
            .unwrap()
            .title,
        "Retained channel name"
    );
    assert_eq!(
        app.shared_views.chrome_cards_visited.get(),
        app.durable.cards.len()
    );
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
        ModelEdit::SuggestChannelTitle { id, title, .. } if id == destination && title == "Retained channel name")));
    drop(app);
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_inactive_scope_refreshes_changed_active_primary_chrome() {
    let (mut app, source, destination, pane) = presentation_fixture();
    let make_chat = replica_capture::retained_chat;
    let first = make_chat("First chat");
    let second = make_chat("Second chat");
    let chats = vec![
        Pane::from_card(&first, None).unwrap(),
        Pane::from_card(&second, None).unwrap(),
    ];
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.extend([first.clone(), second.clone()]);
    let active = durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == destination)
        .unwrap();
    active.cards = vec![first.id, second.id];
    active.title = "Manual conversation name".into();
    active.title_source = crate::model::ChannelTitleSource::Manual;
    durable.active_channel = Some(destination);
    durable.focused = Some(second.id);
    app.active_workspace = Some(PathBuf::from("/repo"));
    let channel = app
        .workspace_model
        .channels
        .iter_mut()
        .find(|channel| channel.id == destination)
        .unwrap();
    channel.title = "Manual conversation name".into();
    channel.title_source = crate::model::ChannelTitleSource::Manual;
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap()
        .panes = chats.clone();
    app.shared_views
        .initialize(Scope::Channel(destination), chats);
    app.shared_views.attached.extend([first.id, second.id]);
    app.cards.insert(first.id, CardRuntime::Completed);
    app.cards.insert(second.id, CardRuntime::Completed);
    let _directory = replica_capture::writer_fixture(&mut app);
    let captures = app.replica_capture.captures;
    let mut strip = (*app.durable).clone();
    strip.cards = vec![second.clone(), first.clone()].into();
    app.workspace = Cagents::rebuild_workspace(&strip);
    drop(app.sync_chrome());
    assert_eq!(app.card_chrome[&first.id].title, "Manual conversation name");
    assert_eq!(app.card_chrome[&second.id].title, "Second chat");
    app.shared_views.chrome_cards_visited.set(0);
    app.shared_views.selected_projection_count.set(0);
    app.reported_title_save.cards.insert(pane.id);
    app.card_state_mut(pane.id).unwrap().title = "Held local title".into();
    let update = presentation_edge(&app, Scope::Channel(source), 24, "Inactive browser", 0.61);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.selected_projection_count.get(), 1);
    assert!(app.replica_capture.pending.is_none());
    assert_eq!(app.replica_capture.captures, captures + 1);
    assert!(app.reported_title_save.cards.contains(&pane.id));
    assert_eq!(app.shared_views.chrome_cards_visited.get(), 3);
    assert_eq!(app.card_chrome[&first.id].title, "First chat");
    assert_eq!(
        app.card_chrome[&second.id].title,
        "Manual conversation name"
    );
    assert_eq!(app.card_state(pane.id).unwrap().title, "Held local title");
    assert_eq!(app.card_chrome[&pane.id].title, browser_host::TITLE);
    let observed = (
        app.card_chrome[&first.id].title.clone(),
        app.card_chrome[&second.id].title.clone(),
        app.core.window.header_title.clone(),
    );
    drop(app.sync_chrome());
    assert_eq!(
        observed,
        (
            app.card_chrome[&first.id].title.clone(),
            app.card_chrome[&second.id].title.clone(),
            app.core.window.header_title.clone()
        )
    );
    drop(app);
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_preserves_managed_attention_and_later_snapshot_marks_new_evidence() {
    let (mut app, source, channel, _) = presentation_fixture();
    let card = CardId::new();
    let notice = presentation_managed_notice(&mut app, channel, card);
    let first_notice = notice.id;
    drop(app.update_notifications(notification_ui::Action::Snapshot(
        crate::notifications::Snapshot {
            notices: vec![notice.clone()],
            diagnostic: None,
        },
    )));
    assert!(app.sidebar_state.has_card_update(card));
    let attention = app.sidebar_state.card_attention(card);
    let runtime = app.cards.get(&card).unwrap() as *const CardRuntime;
    app.shared_views.notification_attribution_count.set(0);
    let update = presentation_edge(&app, Scope::Channel(source), 30, "Presentation", 0.7);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.notification_attribution_count.get(), 0);
    assert_eq!(app.sidebar_state.card_attention(card), attention);
    assert_eq!(app.cards.get(&card).unwrap() as *const CardRuntime, runtime);
    assert!(!app.notification_ui.acknowledged_for_test(first_notice));
    app.sidebar_state.forget_card_attention(card);
    let mut later = notice;
    later.id = uuid::Uuid::new_v4();
    later.event.evidence.key = "later-managed-failure".into();
    let later_id = later.id;
    drop(app.update_notifications(notification_ui::Action::Snapshot(
        crate::notifications::Snapshot {
            notices: vec![later],
            diagnostic: None,
        },
    )));
    assert_eq!(app.shared_views.notification_attribution_count.get(), 1);
    assert!(app.sidebar_state.has_card_update(card));
    assert_eq!(app.sidebar_state.card_attention(card), attention);
    assert!(!app.notification_ui.acknowledged_for_test(later_id));
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_keeps_duplicate_notification_owners_failclosed_until_full_metadata_repair() {
    use super::super::workspace_model_sync::{EdgeChanges, ModelUpdate};
    use crate::workspace_model::delta::EditEffects;
    let (mut app, source, channel, _) = presentation_fixture();
    let card = CardId::new();
    let notice = presentation_managed_notice(&mut app, channel, card);
    let notice_id = notice.id;
    let mut duplicate = app.workspace_model.agents[0].clone();
    duplicate.id = crate::model::AgentId::new();
    let duplicate_id = duplicate.id;
    app.workspace_model.agents.push(duplicate);
    drop(app.update_notifications(notification_ui::Action::Snapshot(
        crate::notifications::Snapshot {
            notices: vec![notice],
            diagnostic: None,
        },
    )));
    assert!(!app.sidebar_state.has_card_update(card));
    app.shared_views.notification_attribution_count.set(0);
    let update = presentation_edge(
        &app,
        Scope::Channel(source),
        31,
        "No ownership change",
        0.65,
    );
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.notification_attribution_count.get(), 0);
    assert!(!app.sidebar_state.has_card_update(card));
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Changed {
            sequence: 32,
            edit: ModelEdit::AgentsChanged,
            changes: EdgeChanges {
                identity: Some(app.workspace_model.identity),
                effects: EditEffects {
                    removed_agents: vec![duplicate_id],
                    ..Default::default()
                },
                ..Default::default()
            },
        })),
    );
    assert!(
        app.shared_views.notification_attribution_count.get() > 0,
        "actual owner metadata repair retains full attribution"
    );
    assert!(
        app.sidebar_state.has_card_update(card),
        "retained notice is attributed without another journal delivery"
    );
    assert!(!app.notification_ui.acknowledged_for_test(notice_id));
}

fn listed_passive_release_fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
    let (mut app, channel, other, mut pane) = fixture();
    pane.kind = moving_terminal_kind();
    Arc::make_mut(&mut app.durable).cards.push(CardState {
        id: pane.id,
        generation: 0,
        title: pane.title.clone(),
        width_fraction: pane.width_fraction,
        existence: Existence::Open,
        kind: pane.kind.clone(),
    });
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == channel)
        .unwrap()
        .cards
        .push(pane.id);
    app.workspace_model.views.push(View {
        scope: Scope::Channel(channel),
        revision: 1,
        panes: vec![pane.clone()],
        retired: vec![],
    });
    app.cards.insert(pane.id, CardRuntime::Loading);
    app.register_presence(pane.id, Exposure::Offscreen, Residency::Cold);
    app.shared_views.attached.insert(pane.id);
    app.shared_views
        .initialized
        .remove(&Scope::Channel(channel));
    (app, channel, other, pane)
}

#[test]
fn presentation_release_of_a_listed_passive_terminal_needs_no_global_admission() {
    for retired in [false, true] {
        let (mut app, channel, _, pane) = listed_passive_release_fixture();
        if retired {
            app.workspace_model.views[0].retired.push(pane.id);
        }
        let durable = app.durable.clone();
        let attached = app.shared_views.attached.clone();
        let runtime = app.cards.get(&pane.id).unwrap() as *const CardRuntime;
        app.shared_views.scope_discovery_count.set(0);
        let task = app.release_held_terminals();
        if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
            use cosmic::iced::futures::StreamExt;
            assert!(cosmic::iced::futures::executor::block_on(stream.next()).is_none());
        }
        assert_eq!(app.shared_views.scope_discovery_count.get(), 0);
        assert!(
            !app.shared_views
                .initialized
                .contains(&Scope::Channel(channel))
        );
        assert!(Arc::ptr_eq(&durable, &app.durable));
        assert_eq!(app.shared_views.attached, attached);
        assert_eq!(
            app.cards.get(&pane.id).unwrap() as *const CardRuntime,
            runtime
        );
        assert!(matches!(
            app.cards.get(&pane.id),
            Some(CardRuntime::Loading)
        ));
        assert_eq!(app.residency(pane.id), &Residency::Cold);
        assert!(app.saved_terminals.entries.is_empty());
        assert!(app.restored_agent_attaches.is_empty());
        assert!(!app.terminal_start_in_flight(pane.id));
        assert!(app.durable.pending_terminal_creations.is_empty());
    }
}

/// A structural pass decides whether an unlisted held terminal's view
/// has been admitted from the topology it already built, rather than
/// rebuilding roots and authority in its release tail.
#[test]
fn a_structural_pass_releases_held_terminals_through_its_own_topology() {
    let (mut app, _, other, listed) = listed_passive_release_fixture();
    let mut unlisted = listed.clone();
    unlisted.id = CardId::new();
    unlisted.kind = moving_terminal_kind();
    Arc::make_mut(&mut app.durable).cards.push(CardState {
        id: unlisted.id,
        generation: 0,
        title: unlisted.title.clone(),
        width_fraction: unlisted.width_fraction,
        existence: Existence::Open,
        kind: unlisted.kind.clone(),
    });
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == other)
        .unwrap()
        .cards
        .push(unlisted.id);
    app.cards.insert(unlisted.id, CardRuntime::Loading);
    app.register_presence(unlisted.id, Exposure::Offscreen, Residency::Cold);
    app.shared_views.attached.insert(unlisted.id);
    // The discovery a complete pass publishes through is kept between passes,
    // so its topology is built once rather than for each.
    app.prime_view_discovery();
    app.shared_views.topology_build_count.set(0);
    app.shared_views.held_descriptor_clones.set(0);
    drop(app.reconcile_shared_views());
    assert!(
        app.shared_views.held_descriptor_clones.get() > 0,
        "the pass reaches its release tail with the unlisted terminal held"
    );
    assert!(!app.listed_in_registry(unlisted.id));
    assert_eq!(app.shared_views.topology_build_count.get(), 1);
}

#[test]
fn presentation_listed_passive_terminal_does_not_bypass_an_unlisted_cold_barrier() {
    let (mut app, channel, other, listed) = listed_passive_release_fixture();
    let mut unlisted = listed.clone();
    unlisted.id = CardId::new();
    unlisted.kind = moving_terminal_kind();
    Arc::make_mut(&mut app.durable).cards.push(CardState {
        id: unlisted.id,
        generation: 0,
        title: unlisted.title.clone(),
        width_fraction: unlisted.width_fraction,
        existence: Existence::Open,
        kind: unlisted.kind.clone(),
    });
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == other)
        .unwrap()
        .cards
        .push(unlisted.id);
    app.cards.insert(unlisted.id, CardRuntime::Loading);
    app.register_presence(unlisted.id, Exposure::Offscreen, Residency::Cold);
    app.shared_views.attached.insert(unlisted.id);
    let listed_runtime = app.cards.get(&listed.id).unwrap() as *const CardRuntime;
    app.shared_views.scope_discovery_count.set(0);
    drop(app.release_held_terminals());
    assert_eq!(app.shared_views.scope_discovery_count.get(), 1);
    assert!(app.shared_views.attached.contains(&listed.id));
    assert!(app.shared_views.attached.contains(&unlisted.id));
    assert!(app.saved_terminals.entries.is_empty());
    assert!(!app.terminal_start_in_flight(listed.id));
    assert!(!app.terminal_start_in_flight(unlisted.id));

    app.shared_views.initialized.insert(Scope::Channel(channel));
    app.shared_views.scope_discovery_count.set(0);
    drop(app.release_held_terminals());
    assert_eq!(app.shared_views.scope_discovery_count.get(), 1);
    assert!(app.shared_views.attached.contains(&listed.id));
    assert!(!app.shared_views.attached.contains(&unlisted.id));
    assert!(!app.saved_terminals.entries.contains_key(&listed.id));
    assert!(app.saved_terminals.entries.contains_key(&unlisted.id));
    assert_eq!(
        app.cards.get(&listed.id).unwrap() as *const CardRuntime,
        listed_runtime
    );
    assert_eq!(app.residency(listed.id), &Residency::Cold);
    assert_eq!(app.residency(unlisted.id), &Residency::Hibernated);
    assert!(!app.terminal_start_in_flight(listed.id));
    assert!(app.durable.pending_terminal_creations.is_empty());
    assert_eq!(app.card_state(listed.id).unwrap().kind, listed.kind);
    assert_eq!(app.card_state(unlisted.id).unwrap().kind, unlisted.kind);
}

#[test]
fn presentation_release_of_a_held_terminal_still_requires_scope_admission() {
    let (mut app, channel, _, mut pane) = fixture();
    pane.kind = moving_terminal_kind();
    Arc::make_mut(&mut app.durable).cards.push(CardState {
        id: pane.id,
        generation: 0,
        title: pane.title.clone(),
        width_fraction: pane.width_fraction,
        existence: Existence::Open,
        kind: pane.kind.clone(),
    });
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == channel)
        .unwrap()
        .cards
        .push(pane.id);
    app.cards.insert(pane.id, CardRuntime::Loading);
    app.register_presence(pane.id, Exposure::Offscreen, Residency::Cold);
    app.shared_views.attached.insert(pane.id);
    app.shared_views
        .initialized
        .remove(&Scope::Channel(channel));
    app.shared_views.scope_discovery_count.set(0);
    drop(app.release_held_terminals());
    assert_eq!(app.shared_views.scope_discovery_count.get(), 1);
    assert!(app.shared_views.attached.contains(&pane.id));
    assert!(!app.saved_terminals.entries.contains_key(&pane.id));
    assert!(!app.terminal_start_in_flight(pane.id));
    assert_eq!(app.residency(pane.id), &Residency::Cold);

    app.shared_views.initialized.insert(Scope::Channel(channel));
    app.shared_views.scope_discovery_count.set(0);
    drop(app.release_held_terminals());
    assert_eq!(app.shared_views.scope_discovery_count.get(), 1);
    assert!(!app.shared_views.attached.contains(&pane.id));
    assert!(
        app.saved_terminals.entries.contains_key(&pane.id),
        "admitted absence releases the existing read-only saved-terminal inspection"
    );
    assert_eq!(app.residency(pane.id), &Residency::Hibernated);
    assert!(
        app.durable.pending_terminal_creations.is_empty(),
        "passive release never creates a new terminal launch intention"
    );
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
}

#[test]
fn presentation_release_of_an_opening_terminal_keeps_its_admission_bypass() {
    let (mut app, channel, _, mut pane) = fixture();
    pane.kind = moving_terminal_kind();
    Arc::make_mut(&mut app.durable).cards.push(CardState {
        id: pane.id,
        generation: 0,
        title: pane.title.clone(),
        width_fraction: pane.width_fraction,
        existence: Existence::Open,
        kind: pane.kind.clone(),
    });
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == channel)
        .unwrap()
        .cards
        .push(pane.id);
    app.workspace_model.views.push(View {
        scope: Scope::Channel(channel),
        revision: 1,
        panes: vec![pane.clone()],
        retired: vec![],
    });
    app.cards.insert(pane.id, CardRuntime::Loading);
    app.register_presence(
        pane.id,
        Exposure::Visible,
        Residency::Hydrating { epoch: 1 },
    );
    app.shared_views.attached.insert(pane.id);
    app.shared_views
        .initialized
        .remove(&Scope::Channel(channel));
    app.shared_views.scope_discovery_count.set(0);
    drop(app.release_held_terminals());
    assert_eq!(
        app.shared_views.scope_discovery_count.get(),
        0,
        "opening already bypasses global scope admission"
    );
    assert!(
        !app.shared_views
            .initialized
            .contains(&Scope::Channel(channel))
    );
    assert_eq!(app.residency(pane.id), &Residency::Cold);
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Loading)
    ));
    assert!(app.shared_views.attached.contains(&pane.id));
    assert!(!app.saved_terminals.entries.contains_key(&pane.id));
    assert!(!app.terminal_start_in_flight(pane.id));
    assert!(app.durable.pending_terminal_creations.is_empty());
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_projection_rejects_ahead_structure_aliases_metadata_and_transfer() {
    use super::super::workspace_model_sync::ModelUpdate;
    let (mut app, source, destination, pane) = presentation_fixture();
    let scope = Scope::Channel(source);
    let mut update = presentation_edge(&app, scope.clone(), 22, "Ahead", 0.7);
    let ModelUpdate::Changed { changes, .. } = &mut update else {
        unreachable!()
    };
    let mut added = pane.clone();
    added.id = CardId::new();
    if let CardKind::Browser(browser) = &mut added.kind {
        browser.id = BrowserId::new();
    }
    changes.views.views[0].panes.push(added.clone());
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.selected_projection_count.get(), 0);
    assert_eq!(app.shared_views.local_projection_count.get(), 1);
    assert!(
        app.card_state(added.id).is_some(),
        "an ahead scoped read projects its new member through the full path"
    );

    let before = app
        .workspace_model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .clone();
    let mut after = before.clone();
    after.revision += 1;
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap()
        .panes
        .push(pane.clone());
    assert_eq!(
        app.presentation_projection_ready(&before, &after, None),
        Err("canonical ownership is ambiguous")
    );
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(destination))
        .unwrap()
        .panes
        .clear();
    let mut alias = app.card_state(pane.id).unwrap().clone();
    alias.id = CardId::new();
    Arc::make_mut(&mut app.durable).cards.push(alias);
    assert_eq!(
        app.presentation_projection_ready(&before, &after, None),
        Err("a local resource alias needs reconciliation")
    );
    let last = app.durable.cards.len() - 1;
    Arc::make_mut(&mut app.durable).cards.remove(last);
    let retained_failure = app.cards.remove(&pane.id).unwrap();
    app.cards.insert(pane.id, CardRuntime::Loading);
    assert_eq!(
        app.presentation_projection_ready(&before, &after, None),
        Err("pane source or runtime is unsettled")
    );
    app.cards.insert(pane.id, retained_failure);
    app.shared_views
        .requested_transfers
        .insert(uuid::Uuid::new_v4(), pane.id);
    assert_eq!(
        app.presentation_projection_ready(&before, &after, None),
        Err("workspace lifecycle is unsettled")
    );
    app.shared_views.requested_transfers.clear();
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == source)
        .unwrap()
        .title = "Optimistic metadata".into();
    assert_eq!(
        app.presentation_projection_ready(&before, &after, None),
        Err("channel metadata is unsettled")
    );
    let mut worktree = before;
    worktree.scope = Scope::Worktree {
        repository: PathBuf::from("/repo/.git"),
        path: PathBuf::from("/repo"),
    };
    assert_eq!(
        app.presentation_projection_ready(&worktree, &worktree, None),
        Err("derived worktree membership")
    );
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_projection_matches_full_projection_and_preserves_unrelated_runtime() {
    let (mut selected, source, _, pane) = presentation_fixture();
    let mut full = super::super::tests::context_inspector_test_app(false);
    full.durable = selected.durable.clone();
    full.workspace_model = selected.workspace_model.clone();
    full.sidebar_model.projects = selected.sidebar_model.projects.clone();
    full.workspace = Cagents::rebuild_workspace(&full.durable);
    full.shared_views.initialized = selected.shared_views.initialized.clone();
    drop(full.reconcile_workspace_model());
    let (selected_tx, selected_rx) = test_model_channel();
    selected.workspace_model_tx = selected_tx;
    let (full_tx, full_rx) = test_model_channel();
    full.workspace_model_tx = full_tx;
    let failure = match selected.cards.get(&pane.id) {
        Some(CardRuntime::Failed(error)) => format!("{error:?}"),
        _ => panic!("browserless unavailable surface"),
    };
    let residency = selected.residency(pane.id).clone();
    selected.shared_views.pane_attachment_count = 0;
    full.shared_views.pane_attachment_count = 0;
    let update = presentation_edge(
        &selected,
        Scope::Channel(source),
        24,
        "New remote title",
        0.63,
    );
    // Force the reference through the complete path with an unchanged
    // canonical metadata row. It carries the same accepted scope.
    let mut reference = update.clone();
    if let super::super::workspace_model_sync::ModelUpdate::Changed { changes, .. } = &mut reference
    {
        changes
            .effects
            .channels
            .push(full.workspace_model.channel(source).unwrap().clone());
    }
    let runtime = selected.cards.get(&pane.id).unwrap() as *const CardRuntime;
    drop(selected.update(Message::WorkspaceModelObserved(update)));
    drop(full.update(Message::WorkspaceModelObserved(reference)));
    assert_eq!(selected.card_state(pane.id), full.card_state(pane.id));
    let selected_chrome = &selected.card_chrome[&pane.id];
    let full_chrome = &full.card_chrome[&pane.id];
    assert_eq!(
        (
            selected_chrome.title.as_str(),
            selected_chrome.caption.as_str(),
            selected_chrome.focused_name.as_str(),
            selected_chrome.title_width
        ),
        (
            full_chrome.title.as_str(),
            full_chrome.caption.as_str(),
            full_chrome.focused_name.as_str(),
            full_chrome.title_width
        )
    );
    assert_eq!(
        selected.core.window.header_title,
        full.core.window.header_title
    );
    assert_eq!(
        selected.workspace.card(pane.id).unwrap().accessible_name,
        full.workspace.card(pane.id).unwrap().accessible_name
    );
    assert_eq!(
        selected.workspace.order().collect::<Vec<_>>(),
        full.workspace.order().collect::<Vec<_>>()
    );
    assert_eq!(selected.durable.channels, full.durable.channels);
    assert_eq!(selected.shared_views.baseline, full.shared_views.baseline);
    assert_eq!(
        selected.cards.get(&pane.id).unwrap() as *const CardRuntime,
        runtime
    );
    let published =
        |rx: TestModelRequests| std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(published(selected_rx), published(full_rx));
    assert!(
        matches!(selected.cards.get(&pane.id), Some(CardRuntime::Failed(error)) if format!("{error:?}") == failure)
    );
    assert!(
        matches!(full.cards.get(&pane.id), Some(CardRuntime::Failed(error)) if format!("{error:?}") == failure)
    );
    assert_eq!(selected.residency(pane.id), &residency);
    assert_eq!(selected.residency(pane.id), full.residency(pane.id));
    assert_eq!(selected.shared_views.pane_attachment_count, 0);
    assert_eq!(full.shared_views.pane_attachment_count, 0);
    assert!(!selected.terminal_start_in_flight(pane.id));
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_projection_preserves_an_unrelated_draft_and_pending_history() {
    let (mut app, source, destination, pane) = presentation_fixture();
    drop(app.select_channel(destination));
    assert_eq!(app.durable.active_channel, Some(destination));
    assert!(app.workspace.order().next().is_none());
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    // The placeholder is saved with the navigation that opened it.
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    let draft = app.workspace.focused().unwrap();
    app.retain_local_pane(draft);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&draft) else {
        panic!("draft runtime")
    };
    let target = runtime.target;
    compose_default_chat(&mut app, target);
    let draft_state = app.card_state(draft).unwrap().clone();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&draft) else {
        panic!("draft runtime")
    };
    let text = runtime.unsent_text();
    let runtime_address = runtime.as_ref() as *const super::super::chat_ui::Runtime;
    let historical = CardId::new();
    app.shared_views
        .restorations
        .push(crate::persistence::TerminalPaneRestoration {
            canonical: historical,
            terminal: TerminalId::new(),
        });
    let update = presentation_edge(&app, Scope::Channel(source), 25, "Independent update", 0.74);
    app.shared_views.selected_projection_count.set(0);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(
        app.shared_views.selected_projection_count.get(),
        1,
        "unrelated retained history is not a scope admission barrier"
    );
    assert_eq!(app.card_state(draft), Some(&draft_state));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&draft) else {
        panic!("draft runtime")
    };
    assert_eq!(
        runtime.as_ref() as *const super::super::chat_ui::Runtime,
        runtime_address
    );
    assert_eq!(runtime.unsent_text(), text);
    assert!(
        app.shared_views
            .restorations
            .iter()
            .any(|row| row.canonical == historical)
    );
    assert_eq!(app.card_state(pane.id).unwrap().title, "Independent update");
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_receipt_and_observer_echo_are_bounded_in_either_order() {
    use super::super::workspace_model_sync::ModelUpdate;
    use crate::workspace_model::delta::EditReceipt;
    for observer_first in [false, true] {
        let (mut app, source, _, pane) = presentation_fixture();
        let observer =
            presentation_edge(&app, Scope::Channel(source), 27, "My accepted label", 0.69);
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
        app.card_state_mut(pane.id).unwrap().title = "My accepted label".into();
        app.card_state_mut(pane.id).unwrap().width_fraction = 0.69;
        let _ = app
            .workspace
            .set_card_width(pane.id, CardWidth::Fraction(0.69));
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        drop(app.sync_chrome());
        app.shared_views.chrome_cards_visited.set(0);
        app.shared_views.publisher_index_entries = 0;
        app.shared_views.local_projection_count.set(0);
        app.shared_views.selected_projection_count.set(0);
        app.shared_views.structural_tail_count = 0;
        app.shared_views.chrome_tail_count = 0;
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
            observer_first,
            "only the exact receipt acknowledges local intent"
        );
        drop(app.update(Message::WorkspaceModelObserved(second)));
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, _, _)| *id == operation)
        );
        assert_eq!(app.shared_views.local_projection_count.get(), 0);
        assert_eq!(app.shared_views.selected_projection_count.get(), 2);
        assert_eq!(app.shared_views.structural_tail_count, 0);
        assert_eq!(app.shared_views.chrome_tail_count, 2);
        assert_eq!(app.shared_views.chrome_cards_visited.get(), 2);
        assert_eq!(app.shared_views.publisher_index_entries, 2);
        assert_eq!(app.card_state(pane.id).unwrap().title, "My accepted label");
        assert_eq!(app.card_state(pane.id).unwrap().width_fraction, 0.69);
        assert!(rx.try_recv().is_err());
        let held = app
            .workspace_model
            .views
            .iter()
            .find(|view| view.scope == Scope::Channel(source))
            .unwrap();
        let mut contradictory = held.clone();
        contradictory.panes[0].title = "Different same revision".into();
        assert_eq!(
            app.presentation_projection_ready(held, &contradictory, None),
            Err("scope structure or source changed")
        );
    }
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_duplicate_local_placement_uses_full_recovery_and_keeps_active_draft() {
    let (mut app, source, destination, pane) = presentation_fixture();
    drop(app.select_channel(destination));
    assert_eq!(app.durable.active_channel, Some(destination));
    assert!(app.workspace.order().next().is_none());
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    // The placeholder is saved with the navigation that opened it.
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    let draft = app.workspace.focused().unwrap();
    app.retain_local_pane(draft);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&draft) else {
        panic!("draft runtime")
    };
    let target = runtime.target;
    compose_default_chat(&mut app, target);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&draft) else {
        panic!("draft runtime")
    };
    let draft_address = runtime.as_ref() as *const super::super::chat_ui::Runtime;
    let draft_text = runtime.unsent_text();
    let draft_state = app.card_state(draft).unwrap().clone();
    let failure = match app.cards.get(&pane.id) {
        Some(CardRuntime::Failed(error)) => format!("{error:?}"),
        _ => panic!("unavailable browser surface"),
    };
    let surface_address = app.cards.get(&pane.id).unwrap() as *const CardRuntime;
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == destination)
        .unwrap()
        .cards
        .push(pane.id);
    let held = app
        .workspace_model
        .views
        .iter()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .clone();
    let mut next = held.clone();
    next.revision += 1;
    assert_eq!(
        app.presentation_projection_ready(&held, &next, None),
        Err("local pane placement is ambiguous")
    );
    let update = presentation_edge(
        &app,
        Scope::Channel(source),
        28,
        "Recovered placement",
        0.67,
    );
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    app.shared_views.pane_attachment_count = 0;
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.local_projection_count.get(), 1);
    assert_eq!(app.shared_views.selected_projection_count.get(), 0);
    assert!(
        app.durable
            .channels
            .iter()
            .find(|row| row.id == source)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|row| row.id == destination)
            .unwrap()
            .cards,
        vec![draft]
    );
    assert_eq!(app.durable.active_channel, Some(destination));
    assert_eq!(app.workspace.focused(), Some(draft));
    assert_eq!(
        app.card_state(pane.id).unwrap().title,
        "Recovered placement"
    );
    assert!(
        matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(error)) if format!("{error:?}") == failure)
    );
    assert_eq!(
        app.cards.get(&pane.id).unwrap() as *const CardRuntime,
        surface_address
    );
    assert_eq!(app.shared_views.pane_attachment_count, 0);
    assert_eq!(app.card_state(draft), Some(&draft_state));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&draft) else {
        panic!("draft runtime")
    };
    assert_eq!(
        runtime.as_ref() as *const super::super::chat_ui::Runtime,
        draft_address
    );
    assert_eq!(runtime.unsent_text(), draft_text);
    assert!(rx.try_recv().is_err());
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn presentation_remote_width_with_a_pending_local_title_uses_the_full_overlay() {
    let (mut app, source, _, pane) = presentation_fixture();
    let scope = Scope::Channel(source);
    let update = presentation_edge(&app, scope.clone(), 26, &pane.title, 0.76);
    let mut local = pane.clone();
    local.title = "My pending title".into();
    let operation = uuid::Uuid::new_v4();
    app.shared_views.pending.push((
        operation,
        scope,
        Change::Update {
            before: pane.clone(),
            pane: local.clone(),
        },
    ));
    app.card_state_mut(pane.id).unwrap().title = local.title.clone();
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    drop(app.update(Message::WorkspaceModelObserved(update)));
    assert_eq!(app.shared_views.local_projection_count.get(), 1);
    assert_eq!(app.shared_views.selected_projection_count.get(), 0);
    assert_eq!(app.card_state(pane.id).unwrap().title, "My pending title");
    assert_eq!(app.card_state(pane.id).unwrap().width_fraction, 0.76);
    assert!(
        app.shared_views
            .pending
            .iter()
            .any(|(id, _, _)| *id == operation)
    );
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn committed_pending_view_edit_projects_once_after_acknowledgment() {
    use super::super::workspace_model_sync::{EdgeChanges, ModelUpdate};
    use crate::workspace_model::delta::{EditEffects, EditReceipt, ViewChanges, ViewRevision};
    let (mut app, source, _, pane) = presentation_fixture();
    let scope = Scope::Channel(source);
    let operation = uuid::Uuid::new_v4();
    let mut changed = pane.clone();
    changed.title = "Accepted title".into();
    changed.width_fraction = 0.68;
    let change = Change::Update {
        before: pane,
        pane: changed.clone(),
    };
    app.shared_views
        .pending
        .push((operation, scope.clone(), change.clone()));
    app.card_state_mut(changed.id).unwrap().title = changed.title.clone();
    app.card_state_mut(changed.id).unwrap().width_fraction = changed.width_fraction;
    let _ = app
        .workspace
        .set_card_width(changed.id, CardWidth::Fraction(changed.width_fraction));
    let mut accepted = app
        .workspace_model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .clone();
    accepted.apply(&change).unwrap();
    accepted.revision = 18;
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    drop(app.apply_workspace_model_update(ModelUpdate::Committed {
        receipt: EditReceipt {
            identity: app.workspace_model.identity,
            sequence: 18,
            effects: EditEffects {
                views: vec![ViewRevision {
                    scope: scope.clone(),
                    before: Some(17),
                    after: Some(18),
                }],
                ..Default::default()
            },
        },
        edit: ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: 17,
            operation,
            change,
        },
        views: ViewChanges {
            sequence: 18,
            views: vec![accepted.clone()],
            ..Default::default()
        },
    }));
    assert_eq!(
        app.shared_views.local_projection_count.get()
            + app.shared_views.selected_projection_count.get(),
        1,
        "the receipt installs canonical state, acknowledges its overlay, then takes one real local projection"
    );
    assert_eq!(app.shared_views.local_projection_count.get(), 0);
    assert_eq!(app.shared_views.selected_projection_count.get(), 1);
    assert!(
        !app.shared_views
            .pending
            .iter()
            .any(|(id, _, _)| *id == operation)
    );
    assert_eq!(
        app.workspace_model
            .views
            .iter()
            .find(|view| view.scope == scope),
        Some(&accepted)
    );
    assert_eq!(app.card_state(changed.id).unwrap().title, changed.title);
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|edit| matches!(edit, ModelEdit::ChangeView { .. })),
        "acknowledged metadata is not published again"
    );

    accepted.revision = 19;
    accepted.panes[0].title = "Peer title".into();
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: 19,
        edit: ModelEdit::AgentsChanged,
        changes: EdgeChanges {
            identity: Some(app.workspace_model.identity),
            views: ViewChanges {
                sequence: 19,
                views: vec![accepted],
                ..Default::default()
            },
            ..Default::default()
        },
    }));
    assert_eq!(
        app.shared_views.local_projection_count.get()
            + app.shared_views.selected_projection_count.get(),
        1,
        "an observer change still projects once"
    );
    assert_eq!(app.card_state(changed.id).unwrap().title, "Peer title");
    app.shared_views.local_projection_count.set(0);
    app.shared_views.selected_projection_count.set(0);
    drop(app.apply_workspace_model_update(ModelUpdate::Committed {
        receipt: EditReceipt {
            identity: uuid::Uuid::new_v4(),
            sequence: 900,
            effects: EditEffects::default(),
        },
        edit: ModelEdit::AgentsChanged,
        views: ViewChanges::default(),
    }));
    assert_eq!(
        app.shared_views.local_projection_count.get()
            + app.shared_views.selected_projection_count.get(),
        0,
        "a foreign registry receipt never projects"
    );
}

#[test]
fn reconciliation_settles_an_unsaved_strip_order_before_taking_its_baseline() {
    let (mut app, source, _, pane) = channel_move_fixture();
    let mut next = pane.clone();
    next.id = CardId::new();
    if let CardKind::Browser(browser) = &mut next.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .panes
        .push(next.clone());
    drop(app.reconcile_shared_views());
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let _ = app.workspace.reorder(next.id, 0).unwrap();
    // The next echo projects the view over the mounted strip before the
    // snapshot, so the snapshot has no reorder for the baseline to absorb.
    drop(app.reconcile_shared_views());
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        vec![pane.id, next.id]
    );
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards,
        vec![pane.id, next.id]
    );
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|edit| matches!(edit, ModelEdit::ChangeView { .. }))
    );
}

/// Scope discovery by scanning every root and project worktree per card
/// and path: the definition the indexed discovery must reproduce.
fn reference_view_scopes(app: &Cagents) -> Vec<Scope> {
    let mut scopes = app
        .durable
        .channels
        .iter()
        .map(|channel| Scope::Channel(channel.id))
        .collect::<Vec<_>>();
    scopes.extend(app.workspace_model.views.iter().filter_map(|view| {
        match &view.scope {
            Scope::Worktree { repository, .. }
                if app
                    .sidebar_model
                    .projects
                    .iter()
                    .any(|project| &project.repository == repository) =>
            {
                Some(view.scope.clone())
            }
            _ => None,
        }
    }));
    let paths = app
        .known_worktree_roots
        .iter()
        .map(PathBuf::as_path)
        .chain(app.active_workspace.as_deref())
        .chain(
            app.durable
                .cards
                .iter()
                .filter_map(|card| app.card_worktree(card)),
        );
    scopes.extend(paths.filter_map(|path| app.worktree_view_scope(path)));
    let mut seen = HashSet::new();
    scopes.retain(|scope| seen.insert(scope.clone()));
    scopes
}

#[test]
fn batched_projection_preserves_large_workspace_history_and_pending_starts() {
    let (mut app, channel, _, pane) = fixture();
    let repository = PathBuf::from("/repo/.git");
    let paths = (0..1105)
        .map(|index| PathBuf::from(format!("/repo/worktree-{index}")))
        .collect::<Vec<_>>();
    let worktrees = paths
        .iter()
        .map(|path| {
            crate::sidebar::SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                path.clone(),
                None,
                false,
                false,
                None,
                None,
                None,
            )
        })
        .collect();
    app.sidebar_model.projects.push(SidebarProject::new(
        ProjectId::new(),
        RepositoryId::new(),
        "repo".into(),
        repository.clone(),
        worktrees,
        vec![],
    ));
    app.known_worktree_roots.extend(paths.iter().cloned());
    app.active_workspace = Some(PathBuf::from("/repo"));
    let terminal = fresh_terminal_session(LaunchRecipe::shell(paths[0].clone()).unwrap());
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.clear();
    for (index, path) in paths[..1024].iter().enumerate() {
        let id = CardId::new();
        let existence = if index < 172 {
            Existence::Open
        } else {
            Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 1,
                    reason: Some("historical pane".into()),
                },
            }
        };
        durable.cards.push(CardState {
            id,
            generation: durable.generation,
            title: format!("pane {index}"),
            width_fraction: pane.width_fraction,
            existence,
            kind: if index % 3 == 0 {
                CardKind::Terminal(fresh_terminal_session(
                    LaunchRecipe::shell(path.join("src")).unwrap(),
                ))
            } else {
                pane.kind.clone()
            },
        });
        if index % 3 != 0 {
            durable.card_scopes.insert(id, path.clone());
        }
        if index % 7 == 0 {
            durable.channels[0].cards.push(id);
        }
    }
    let pending_id = durable.cards[0].id;
    // A channel takes precedence over an explicit worktree scope; the host
    // pending-start bit is carried even without a local launch intention.
    let mut pending = pane.clone();
    pending.id = pending_id;
    pending.kind = CardKind::Terminal(terminal);
    pending.pending_terminal_start = true;
    let mut view = View::empty(channel);
    view.panes.push(pending);
    app.workspace_model.views.push(view);
    let retained = app.durable.cards[1].id;
    app.shared_views.retained.insert(retained);
    app.chrome_ui
        .user_titles
        .insert(app.durable.cards[2].id, "custom title".into());

    let started = Instant::now();
    let batched = app.local_views();
    let batched_elapsed = started.elapsed();
    let started = Instant::now();
    let scopes = reference_view_scopes(&app);
    let reference = scopes
        .iter()
        .map(|scope| (scope.clone(), app.local_view_panes(scope)))
        .collect::<HashMap<_, _>>();
    let reference_elapsed = started.elapsed();
    assert_eq!(batched.scopes, scopes);
    assert_eq!(app.local_view_scopes(), scopes);
    assert_eq!(batched.panes, reference);
    for card in app.durable.cards.iter().step_by(17) {
        let listing = scopes
            .iter()
            .filter(|scope| app.local_view_cards(scope).contains(&card.id))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(app.local_scopes_listing(card.id), listing);
    }
    assert!(batched.panes[&Scope::Channel(channel)][0].pending_terminal_start);
    assert!(
        batched.panes[&Scope::Worktree {
            repository,
            path: paths[1023].clone(),
        }]
            .is_empty()
    );
    eprintln!(
        "large workspace projection: indexed {batched_elapsed:?}, reference {reference_elapsed:?}"
    );
}
#[test]
fn shared_layout_echo_preserves_observed_terminal_exit() {
    let (mut app, channel, _, mut pane) = fixture();
    pane.kind = CardKind::Terminal(fresh_terminal_session(
        LaunchRecipe::shell(PathBuf::from("/repo")).unwrap(),
    ));
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    app.workspace_model.views = vec![view.clone()];
    let owned = HashSet::from([pane.id]);
    drop(app.project_shared_view(&view, &owned));
    app.cards.insert(pane.id, CardRuntime::Completed);
    app.retire_ended_terminal(pane.id, 17, None, false);
    let observed = app.ended_terminal(pane.id).unwrap().continuity.clone();

    // The registry's pane descriptor can predate the runtime event, even
    // while its layout revision is newer. It does not revoke that event.
    drop(app.project_shared_view(&view, &owned));
    assert_eq!(app.ended_terminal(pane.id).unwrap().continuity, observed);

    let CardKind::Terminal(incoming) = &mut view.panes[0].kind else {
        panic!()
    };
    incoming.transcript_path = Some(PathBuf::from("/new/transcript"));
    incoming.launch.arguments.push("changed".into());
    let expected_launch = incoming.launch.clone();
    let TerminalContinuity::RelaunchAvailable { recipe, .. } = &mut incoming.continuity else {
        panic!()
    };
    *recipe = expected_launch.clone();
    drop(app.project_shared_view(&view, &owned));
    let saved = app.ended_terminal(pane.id).unwrap();
    assert_eq!(saved.launch, expected_launch);
    assert_eq!(
        saved.transcript_path.as_deref(),
        Some(Path::new("/new/transcript"))
    );
    let TerminalContinuity::RelaunchAvailable {
        last_exit: observed_exit,
        ..
    } = observed
    else {
        panic!()
    };
    assert_eq!(
        saved.continuity,
        TerminalContinuity::RelaunchAvailable {
            recipe: expected_launch,
            last_exit: observed_exit,
        }
    );
}

#[test]
fn shared_projection_does_not_turn_hydrating_prior_exit_into_a_settled_end() {
    let (mut app, channel, _, mut pane) = fixture();
    pane.kind = CardKind::Terminal(fresh_terminal_session(
        LaunchRecipe::shell(PathBuf::from("/repo")).unwrap(),
    ));
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    app.workspace_model.views = vec![view.clone()];
    let owned = HashSet::from([pane.id]);
    drop(app.project_shared_view(&view, &owned));
    app.retire_ended_terminal(pane.id, 17, None, false);
    app.cards.insert(pane.id, CardRuntime::Loading);
    app.set_residency(pane.id, Residency::Hydrating { epoch: 1 });
    assert!(!app.terminal_start_in_flight(pane.id));

    // Projection admits a waiting pane as Cold. Evidence must come from
    // before that change, while the old exit still belongs to a relaunch.
    drop(app.project_shared_view(&view, &owned));
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert!(app.ended_terminal(pane.id).is_none());
}

#[test]
fn shared_exit_overlay_respects_new_identity_proof_and_explicit_end() {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let id = app.durable.cards[0].id;
    app.retire_ended_terminal(id, 17, None, false);
    let card = app.card_state(id).unwrap().clone();
    let CardKind::Terminal(session) = &card.kind else {
        panic!()
    };
    let mut missing = session.clone();
    missing.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: missing.launch.clone(),
        last_exit: None,
    };
    let mut new_identity = missing.clone();
    new_identity.id = TerminalId::new();
    let mut live = missing.clone();
    live.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("replacement"),
        boot_id: crate::model::BootId::new("replacement"),
        host_incarnation: crate::model::HostIncarnation::new("replacement"),
    };
    let mut explicit_end = missing.clone();
    explicit_end.continuity = TerminalContinuity::Exited {
        last_exit: terminal_exit(23, None),
    };
    for session in [new_identity, live, explicit_end] {
        let mut kind = CardKind::Terminal(session);
        let expected = kind.clone();
        preserve_observed_terminal_exit(app.ended_terminal(id), &mut kind);
        assert_eq!(kind, expected);
    }
    app.set_residency(id, Residency::Hydrating { epoch: 1 });
    let mut kind = CardKind::Terminal(missing);
    let expected = kind.clone();
    preserve_observed_terminal_exit(app.ended_terminal(id), &mut kind);
    assert_eq!(
        kind, expected,
        "an in-flight relaunch is not a settled exit"
    );
}

#[test]
fn shared_terminal_reconcile_does_not_block_native_input_on_autosave_contention() {
    fn run(closed_owner: bool) -> Duration {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let mut app = super::super::tests::context_inspector_test_app(true);
        let mut card = app.durable.cards[0].clone();
        let source_id = card.id;
        if closed_owner {
            card.id = CardId::new();
        }
        assert!(matches!(card.kind, CardKind::Terminal(_)));
        let channel = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: channel,
                project_id: ProjectId::new(),
                title: "shared terminal".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: true,
                cards: vec![source_id],
                focused: Some(source_id),
                checkout: None,
            });
        let mut view = View::empty(Scope::Channel(channel));
        view.panes.push(Pane::from_card(&card, None).unwrap());
        app.workspace_model.views.push(view);
        app.shared_views.initialized.insert(Scope::Channel(channel));
        let (runtime, _) = crate::session::SessionRuntime::open(
            &path,
            crate::model::BootId::new("adoption-contention"),
        )
        .unwrap();
        runtime.snapshot(Arc::clone(&app.durable));
        runtime.flush(Duration::from_secs(5)).unwrap();
        let store = runtime.operation_store();
        app.session = Some(runtime);
        if closed_owner {
            let durable = Arc::make_mut(&mut app.durable);
            durable.cards[0].existence = Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 123,
                    reason: Some("already closed historical owner".into()),
                },
            };
            durable.focused = None;
            let navigation = durable
                .channels
                .iter_mut()
                .find(|item| item.id == channel)
                .unwrap();
            navigation.cards.clear();
            navigation.focused = None;
            app.session
                .as_ref()
                .unwrap()
                .snapshot(Arc::clone(&app.durable));
            app.session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(5))
                .unwrap();
            let CardKind::Terminal(terminal) = &card.kind else {
                unreachable!()
            };
            assert!(
                store
                    .closed_terminal_pane_adoption(&app.durable, card.id, terminal.id)
                    .unwrap()
                    .is_some(),
                "fixture needs an actual closed DB recipe owner"
            );
        }
        let (entered, ready) = std::sync::mpsc::channel();
        let (begin_delay, begin) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let mut connection = rusqlite::Connection::open(path).unwrap();
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            entered.send(()).unwrap();
            if begin.recv_timeout(Duration::from_secs(4)).is_ok() {
                std::thread::sleep(Duration::from_millis(600));
            }
            transaction.rollback().unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut pending = (*app.durable).clone();
        pending.cards[0].title = "ordinary pending autosave".into();
        app.session.as_ref().unwrap().snapshot(Arc::new(pending));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !store.audit_writer_holds_connection() {
            assert!(
                Instant::now() < deadline,
                "autosave did not acquire its connection"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        begin_delay.send(()).unwrap();
        // This is the actual synchronous native model-update callback.
        let started = Instant::now();
        drop(app.reconcile_shared_views());
        let elapsed = started.elapsed();
        blocker.join().unwrap();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        eprintln!(
            "shared terminal reconciliation closed_owner={closed_owner} blocked native input for {elapsed:?}"
        );
        if closed_owner {
            let restored = store
                .load_card(card.id, None)
                .unwrap()
                .expect("canonical pane checkpoint lands");
            assert!(restored.existence.is_open());
            assert!(
                !store
                    .load_card(source_id, None)
                    .unwrap()
                    .unwrap()
                    .existence
                    .is_open()
            );
        }
        elapsed
    }
    let normal = run(false);
    let restoration = run(true);
    assert!(
        normal < Duration::from_millis(100) && restoration < Duration::from_millis(100),
        "native model callbacks waited on contended autosave: normal={normal:?}, closed-owner restoration={restoration:?}"
    );
}

#[test]
fn closed_terminal_shared_adoption_preserves_closure_and_both_drafts() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = super::super::tests::context_inspector_test_app(true);
    let mut canonical = app.durable.cards[0].clone();
    let alias = canonical.id;
    canonical.id = CardId::new();
    let pane = Pane::from_card(&canonical, None).unwrap();
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards[0].existence = Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 123,
            reason: Some("closed by user".into()),
        },
    };
    durable.focused = None;
    durable.chat_drafts.insert(alias, "historical draft".into());
    durable
        .chat_drafts
        .insert(canonical.id, "canonical draft".into());
    let before = durable.clone();
    let (runtime, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("closed-owner"),
    )
    .unwrap();
    runtime.snapshot(Arc::clone(&app.durable));
    runtime.flush(Duration::from_secs(5)).unwrap();
    let store = runtime.operation_store();
    app.session = Some(runtime);
    app.prepare_terminal_restoration(&pane);
    app.prepare_terminal_restoration(&pane);
    assert_eq!(app.shared_views.restorations.len(), 1);
    assert!(app.shared_views.adoptions.is_empty());
    assert_eq!(*app.durable, before);
    assert!(app.terminal_starts.lock().unwrap().is_empty());
    Arc::make_mut(&mut app.durable)
        .cards
        .push(canonical.clone());
    app.session.as_ref().unwrap().snapshot_terminal_panes(
        Arc::clone(&app.durable),
        app.shared_views.adoptions.clone(),
        app.shared_views.restorations.clone(),
    );
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        store.load_card(alias, None).unwrap().unwrap().existence,
        before.cards[0].existence
    );
    assert!(
        store
            .load_card(canonical.id, None)
            .unwrap()
            .unwrap()
            .existence
            .is_open()
    );
    assert_eq!(app.durable.chat_drafts, before.chat_drafts);
    app.shared_views.restorations.clear();
    app.prepare_terminal_restoration(&pane);
    assert!(app.shared_views.adoptions.is_empty());
    // Repeated shared-view refreshes retain coalesced autosave behavior.
    // The ordered writer already owns the first restoration checkpoint.
    assert!(app.shared_views.restorations.is_empty());
    let canonical_card = Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|card| card.id == canonical.id)
        .unwrap();
    canonical_card.existence = before.cards[0].existence.clone();
    // A close observed by persistence clears the memo before any later
    // projection can reopen the same canonical identity.
    app.shared_views.prune_restoration_requests(&app.durable);
    assert!(app.shared_views.restoration_requests.is_empty());
    app.prepare_terminal_restoration(&pane);
    assert_eq!(app.shared_views.restorations.len(), 1);
}

#[test]
fn shared_pull_request_adoption_initializes_native_viewer() {
    let (mut app, channel, _, mut pane) = fixture();
    pane.kind = CardKind::PullRequests(crate::model::PullRequestView {
        repository: PathBuf::from("/repo/.git"),
        selected_number: None,
        file_panel_width: None,
        file_panel_collapsed: None,
    });
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::PullRequests(_))
    ));
    assert_eq!(*app.residency(pane.id), Residency::Hot);
}

#[test]
fn remote_shared_native_panes_do_not_initialize_local_viewers() {
    // Device selection is process-wide and cannot be reset. Test the real
    // remote guards without changing the backend of parallel local tests.
    const CHILD: &str = "CAGENTS_TEST_REMOTE_NATIVE_PANES";
    if std::env::var_os(CHILD).is_none() {
        let config = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                concat!(
                    "app::shared_views::tests::",
                    "remote_shared_native_panes_do_not_initialize_local_viewers"
                ),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", config.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    let config = crate::device::configuration_path().unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "devices": [{"name": "test-remote", "host": "localhost", "executable": "cagents"}]
        }))
        .unwrap(),
    )
    .unwrap();
    crate::device::select(Some("test-remote")).unwrap();
    let (mut app, channel, _, mut pane) = fixture();
    let repository = PathBuf::from("/remote-only/project");
    for kind in [
        CardKind::Repository(RepositoryView {
            project: ProjectId::new(),
            repository: repository.clone(),
            worktree: repository.clone(),
            selected_file: Some("file.rs".into()),
        }),
        CardKind::PullRequests(crate::model::PullRequestView {
            repository,
            selected_number: Some(1),
            file_panel_width: None,
            file_panel_collapsed: None,
        }),
    ] {
        pane.kind = kind.clone();
        let mut view = View::empty(channel);
        view.panes.push(pane.clone());
        drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
        assert!(
            matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(reason))
            if reason.contains("unavailable on remote devices"))
        );
        assert_eq!(app.card_state(pane.id).unwrap().kind, kind);
    }
}

#[test]
fn shared_repository_adoption_and_retarget_initialize_native_viewers() {
    let (mut app, channel, _, mut pane) = fixture();
    let root = tempfile::tempdir().unwrap();
    let repository = RepositoryView {
        project: ProjectId::from_uuid(uuid::Uuid::nil()),
        repository: root.path().to_path_buf(),
        worktree: root.path().to_path_buf(),
        selected_file: Some(PathBuf::from("first.rs")),
    };
    pane.kind = CardKind::Repository(repository.clone());
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
        panic!("a shared repository must start its native viewer immediately");
    };
    assert_eq!(runtime.selected_path(), Some(Path::new("first.rs")));
    assert_eq!(*app.residency(pane.id), Residency::Hot);

    let mut replacement = repository;
    replacement.selected_file = Some(PathBuf::from("second.rs"));
    view.panes[0].kind = CardKind::Repository(replacement.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
        panic!("a replacement shared descriptor must start a new viewer");
    };
    assert_eq!(runtime.descriptor, replacement);
    assert_eq!(runtime.selected_path(), Some(Path::new("second.rs")));
}

#[test]
fn checkout_descriptors_keep_local_project_and_live_repository_selection() {
    let (mut app, channel, _, mut pane) = fixture();
    let root = tempfile::tempdir().unwrap();
    let project = ProjectId::new();
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        root.path().join(".git"),
        vec![crate::sidebar::SidebarWorktree::new(
            crate::model::WorktreeId::new(),
            root.path().to_path_buf(),
            Some("main".into()),
            false,
            false,
            None,
            None,
            None,
        )],
        vec![],
    ));
    let descriptor = RepositoryView {
        project,
        repository: root.path().to_path_buf(),
        worktree: root.path().to_path_buf(),
        selected_file: None,
    };
    pane.kind = CardKind::Repository(descriptor.clone());
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    let mut runtime = RepositoryRuntime::file_list_fixture(descriptor.clone(), &["first.rs"]);
    drop(runtime.update(RepositoryAction::OpenFile {
        card: pane.id,
        path: "first.rs".into(),
    }));
    app.cards
        .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
    view.panes[0].kind = views::shared_kind(&pane.kind);
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
        panic!("the local repository runtime must remain attached");
    };
    assert_eq!(runtime.descriptor.project, project);
    assert_eq!(
        runtime.selected_path(),
        Some(Path::new("first.rs")),
        "remapping a window-local project identity must not recreate its live viewer"
    );
}

/// A chat link to `src/lib.rs:12` opens a pane on `src`, which is no
/// project's root. Its own echo must keep the viewer that holds the line.
#[test]
fn a_file_pane_outside_a_project_root_keeps_its_viewer_through_its_echo() {
    let (mut app, channel, _, mut pane) = fixture();
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("src");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("lib.rs"), "fn a() {}\n").unwrap();
    let descriptor = RepositoryView {
        project: ProjectId::new(),
        repository: folder.clone(),
        worktree: folder,
        selected_file: Some("lib.rs".into()),
    };
    pane.kind = CardKind::Repository(descriptor.clone());
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    let mut runtime = RepositoryRuntime::new_directory(descriptor.clone());
    drop(runtime.initial_load(pane.id));
    drop(runtime.preview_fragment(pane.id, "L12"));
    let runtime = Box::new(runtime);
    let viewer = std::ptr::from_ref::<RepositoryRuntime>(&runtime);
    app.cards.insert(pane.id, CardRuntime::Repository(runtime));
    view.panes[0].kind = views::shared_kind(&pane.kind);
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
        panic!("the file pane must stay attached");
    };
    assert!(
        std::ptr::eq(viewer, &**runtime),
        "the pane's own echo must not replace the viewer waiting to open line 12"
    );
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
}

#[test]
fn retargeted_shared_repository_preserves_unsaved_document() {
    let (mut app, channel, _, mut pane) = fixture();
    let runtime = RepositoryRuntime::dirty_fixture(pane.id);
    let original = runtime.descriptor.clone();
    pane.kind = CardKind::Repository(original.clone());
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    app.cards
        .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
    let mut replacement = original.clone();
    replacement.selected_file = Some(PathBuf::from("other.rs"));
    view.panes[0].kind = CardKind::Repository(replacement);
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&pane.id) else {
        panic!("shared metadata must not discard an unsaved document");
    };
    assert!(runtime.is_dirty());
    assert_eq!(runtime.descriptor, original);
}

#[cfg(not(feature = "integrated-browser"))]
#[test]
fn browserless_shared_browser_keeps_saved_descriptor_and_external_open_state() {
    let (mut app, channel, _, pane) = fixture();
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert!(
        matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(reason))
        if reason.contains("Open the saved URL in your system browser"))
    );
}

#[test]
fn a_refused_terminal_creation_closes_instead_of_loading_forever() {
    let (mut app, a, _, first) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let mut view = View::empty(a);
    view.panes.push(first.clone());
    app.workspace_model.views = vec![view.clone()];
    drop(app.project_shared_view(&view, &HashSet::from([first.id])));
    app.shared_views
        .baseline
        .insert(Scope::Channel(a), vec![first.clone()]);
    app.shared_views.pending.clear();
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, Some((0, 0))));
    let id = app.workspace.focused().unwrap();
    assert!(app.durable.pending_terminal_creations.contains(&id));

    app.refuse_shared_view_edit(&Scope::Channel(a), id.as_uuid());
    for _ in 0..2 {
        drop(app.reconcile_shared_views());
    }

    assert!(!app.card_state(id).unwrap().existence.is_open());
    assert!(!app.shared_views.retained.contains(&id));
    assert!(!app.durable.local_only_cards.contains(&id));
    assert!(app.terminal_starts.lock().unwrap().is_empty());
}

#[test]
fn two_clients_project_identical_pane_ids_and_peer_removal() {
    let (mut first, a, _, pane) = fixture();
    let (mut second, _, _, _) = fixture();
    Arc::make_mut(&mut second.durable).channels = first.durable.channels.clone();
    second.shared_views.initialized.insert(Scope::Channel(a));
    let mut view = View::empty(a);
    view.panes.push(pane.clone());
    for app in [&mut first, &mut second] {
        app.workspace_model.views = vec![view.clone()];
        drop(app.reconcile_shared_views());
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|c| c.id == a)
                .unwrap()
                .cards,
            vec![pane.id]
        );
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    }
    for app in [&mut first, &mut second] {
        app.workspace_model.views[0]
            .apply(&Change::Remove { id: pane.id })
            .unwrap();
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    }
}

/// A peer closing the pane this window has focused hands the strip's focus
/// to a neighbour, and nothing else about the strip changes, so no
/// viewport update carries the handoff into the saved state. Storage
/// refuses a workspace that focuses a closed pane, and a refused workspace
/// fails every save barrier until a later one lands.
#[test]
fn a_peer_closing_the_focused_pane_leaves_a_focus_storage_accepts() {
    let (mut app, a, _, first) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let mut second = first.clone();
    second.id = CardId::new();
    if let CardKind::Browser(browser) = &mut second.kind {
        browser.id = BrowserId::new();
    }
    let mut view = View::empty(a);
    view.panes = vec![first.clone(), second.clone()];
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
    let update = app.workspace.focus(second.id).unwrap();
    drop(app.apply_viewport(update, false, DurableChange::None));
    assert_eq!(app.durable.focused, Some(second.id));
    crate::persistence::validate_workspace(&app.persistent_workspace()).unwrap();

    view.apply(&Change::Remove { id: second.id }).unwrap();
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.card_state(second.id).unwrap().existence.is_open());
    assert_eq!(app.workspace.focused(), Some(first.id));
    crate::persistence::validate_workspace(&app.persistent_workspace()).unwrap();
    assert_eq!(
        app.durable.focused,
        Some(first.id),
        "the saved focus follows the strip to the pane that took it"
    );
}

#[test]
fn a_peer_editing_many_panes_rebuilds_the_strip_once() {
    let (mut app, a, _, first) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    drop(app.workspace.resize_viewport(1_200.0));
    let pane = |_| {
        let mut pane = first.clone();
        pane.id = CardId::new();
        if let CardKind::Browser(browser) = &mut pane.kind {
            browser.id = BrowserId::new();
        }
        pane
    };
    let mut view = View::empty(a);
    view.panes = (0..48).map(pane).collect();
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
    let ids = view.panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), ids);
    let update = app.workspace.focus(ids[10]).unwrap();
    drop(app.apply_viewport(update, false, DurableChange::None));

    // Another window closes every other pane, the focused one among them,
    // resizes some, and opens as many new ones, in one snapshot.
    for id in ids.iter().step_by(2) {
        view.apply(&Change::Remove { id: *id }).unwrap();
    }
    let mut panes = view.panes.clone();
    for (index, pane) in panes.iter_mut().enumerate() {
        if index % 3 == 0 {
            pane.width_fraction = 1.0;
        }
    }
    for index in (0..panes.len()).rev().step_by(2) {
        panes.insert(index, pane(index));
    }
    view.panes = panes;
    app.workspace_model.views = vec![view.clone()];
    let (indexed, laid_out, _) = crate::workspace::Workspace::rebuild_counts();
    drop(app.reconcile_shared_views());
    let (indexed_after, laid_out_after, _) = crate::workspace::Workspace::rebuild_counts();
    assert_eq!(indexed_after - indexed, 1, "the strip is indexed once");
    assert_eq!(laid_out_after - laid_out, 1, "the strip is laid out once");

    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        view.panes.iter().map(|pane| pane.id).collect::<Vec<_>>()
    );
    assert!(
        app.workspace
            .cards()
            .iter()
            .zip(&view.panes)
            .all(|(card, pane)| { card.width == CardWidth::Fraction(pane.width_fraction) })
    );
    // The focused pane went; the one after it took focus, and the saved
    // focus followed it.
    assert_eq!(app.workspace.focused(), Some(ids[11]));
    assert_eq!(app.durable.focused, Some(ids[11]));
    assert!(!app.card_state(ids[10]).unwrap().existence.is_open());
    crate::persistence::validate_workspace(&app.persistent_workspace()).unwrap();
}

#[test]
fn transfer_preserves_runtime_when_destination_is_projected_first() {
    let (mut app, a, b, pane) = fixture();
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let owned = HashSet::from([pane.id]);
    let mut source = View::empty(a);
    source.panes.push(pane.clone());
    app.shared_views.applying = true;
    drop(app.project_shared_view(&source, &owned));
    app.shared_views.applying = false;
    let mut target = View::empty(b);
    target.panes.push(pane.clone());
    app.workspace_model.views = vec![target, View::empty(a)];
    drop(app.reconcile_shared_views());
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert!(app.cards.contains_key(&pane.id));
    assert!(
        app.workspace.card(pane.id).is_none(),
        "transferred pane is absent from active source strip"
    );
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|c| c.id == b)
            .unwrap()
            .cards,
        vec![pane.id]
    );
    for scope in [Scope::Channel(a), Scope::Channel(b)] {
        assert_eq!(
            app.shared_views.baseline.get(&scope),
            Some(&app.local_view_panes(&scope)),
            "the baseline describes the completed transfer"
        );
    }
}

#[test]
fn a_transfer_leaves_a_pane_the_user_is_closing_to_that_close() {
    let (mut app, a, b, pane) = fixture();
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let owned = HashSet::from([pane.id]);
    let mut source = View::empty(a);
    source.panes.push(pane.clone());
    app.shared_views.applying = true;
    drop(app.project_shared_view(&source, &owned));
    app.shared_views.applying = false;
    app.workspace.request_close(pane.id).unwrap();

    let mut target = View::empty(b);
    target.panes.push(pane.clone());
    app.workspace_model.views = vec![target, View::empty(a)];
    drop(app.reconcile_shared_views());
    assert!(app.workspace.card(pane.id).is_some());
    assert_eq!(
        app.workspace
            .pending_close()
            .map(|pending| pending.unmounting),
        Some(pane.id)
    );
    app.workspace.commit_close(pane.id).unwrap();
}

#[test]
fn peer_channel_deletion_retains_local_chat_work_in_worktree() {
    let (mut app, a, _, _) = fixture();
    let path = PathBuf::from("/repo");
    app.active_workspace = Some(path.clone());
    app.known_worktree_roots.insert(path.clone());
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    drop(app.new_native_chat(crate::agent::Provider::Codex, Some(path.join("nested"))));
    let card = app.workspace.focused().unwrap();
    let target = chat_ui::Target {
        card,
        generation: 1,
    };
    drop(app.update(Message::ChatUi(chat_ui::Action::Composer(
        target,
        cosmic::widget::text_editor::Action::Edit(cosmic::widget::text_editor::Edit::Paste(
            Arc::new("unsent peer draft".into()),
        )),
    ))));
    drop(app.retire_peer_channel(a));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.cards.contains_key(&card));
    assert!(
        !app.durable
            .channels
            .iter()
            .any(|channel| channel.cards.contains(&card))
    );
    assert_eq!(app.durable.card_scopes.get(&card), Some(&path));
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&card)
            .map(String::as_str),
        Some("unsent peer draft")
    );
}

fn restored_model_only_shared_draft() -> (
    Cagents,
    ChannelId,
    CardId,
    crate::model::ChatDraftModel,
    Pane,
) {
    let provider = crate::agent::Provider::Claude;
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    drop(app.observe_listed_models(
        provider,
        vec![crate::chat::Model {
            id: "haiku".into(),
            label: "Haiku".into(),
            reasoning: vec!["low".into(), "high".into()],
        }],
        None,
    ));
    drop(app.new_native_chat(provider, Some(PathBuf::from("/repo"))));
    let card = app.workspace.focused().unwrap();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("new native draft")
    };
    let target = runtime.target;
    drop(app.update(Message::ChatUi(chat_ui::Action::DraftModel(
        target,
        provider,
        Some("haiku".into()),
    ))));
    drop(app.update(Message::ChatUi(chat_ui::Action::Reasoning(
        target,
        "haiku".into(),
        Some("low".into()),
    ))));
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(app.workspace_model.identity);
    let saved = app.persistent_workspace();
    let chosen = saved.chat_draft_models[&card].clone();
    assert!(!saved.chat_drafts.contains_key(&card));
    assert!(!saved.chat_draft_attachments.contains_key(&card));
    let mut restored = chat_ui::tests::reload_draft_model_workspace(&saved);
    restored.workspace_model = app.workspace_model.clone();
    restored.active_workspace = Some(PathBuf::from("/repo"));
    assert!(restored.chat_awaiting_admission(card));
    let pane = Pane::from_card(restored.card_state(card).unwrap(), None).unwrap();
    restored
        .shared_views
        .initialized
        .insert(Scope::Channel(channel));
    let mut view = View::empty(channel);
    view.panes.push(pane.clone());
    restored.workspace_model.views = vec![view];
    drop(restored.reconcile_shared_views());
    assert!(!restored.chat_awaiting_admission(card));
    assert_eq!(
        restored.persistent_workspace().chat_draft_models[&card],
        chosen
    );
    (restored, channel, card, chosen, pane)
}

/// A model pick is not unsaved work: another window closing the pane closes
/// it here too, with nothing to save or discard.
#[test]
fn peer_removal_closes_a_reopened_draft_with_only_a_model_pick() {
    let (mut app, channel, card, _, _) = restored_model_only_shared_draft();
    let mut view = View::empty(channel);
    view.retired.push(card);
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(!app.shared_views.retained.contains(&card));
    assert!(
        !app.persistent_workspace()
            .chat_draft_models
            .contains_key(&card)
    );
}

/// A shared pane taking a new card identity is not held back by a model pick;
/// the pick moves to the new card with its draft.
#[test]
fn alias_adoption_carries_a_reopened_drafts_model_pick() {
    let (mut app, channel, alias, chosen, mut canonical) = restored_model_only_shared_draft();
    canonical.id = CardId::new();
    let mut view = View::empty(channel);
    view.panes.push(canonical.clone());
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.shared_views.blocked.contains(&Scope::Channel(channel)));
    assert!(
        !app.card_state(alias)
            .is_some_and(|state| state.existence.is_open())
    );
    assert!(app.card_state(canonical.id).unwrap().existence.is_open());
    assert!(
        matches!(app.cards.get(&canonical.id), Some(CardRuntime::Chat(_))),
        "the canonical pane opens its draft"
    );
    // Only a draft that applied the carried pick counts it as its own.
    assert!(!app.chat_is_unused(canonical.id));
    assert_eq!(
        app.persistent_workspace().chat_draft_models[&canonical.id],
        chosen
    );
}

#[test]
fn an_open_pane_losing_its_local_placement_does_not_publish_a_removal() {
    let (mut app, a, _, pane) = fixture();
    app.workspace_model.views = vec![View {
        panes: vec![pane.clone()],
        revision: 1,
        ..View::empty(a)
    }];
    drop(app.reconcile_shared_views());
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    let (sender, published) = test_model_channel();
    app.workspace_model_tx = sender;
    for channel in &mut Arc::make_mut(&mut app.durable).channels {
        channel.cards.retain(|id| *id != pane.id);
    }
    app.publish_view_changes();
    assert!(
        !std::iter::from_fn(|| published.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::ChangeView {
                change: Change::Remove { .. },
                ..
            }
        )),
        "only a close may tell every window to close the pane"
    );
}

#[test]
fn empty_shared_views_keep_unused_defaults_local_until_used() {
    let (mut app, a, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    let card = app.workspace.focused().unwrap();
    assert!(app.chat_is_unused_default(card));
    assert!(app.local_view_panes(&Scope::Channel(a)).is_empty());
    drop(app.project_shared_view(&View::empty(a), &HashSet::new()));
    assert!(app.workspace.card(card).is_some());
    assert!(app.card_state(card).unwrap().existence.is_open());
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(card, "draft".into())]));
    assert_eq!(app.local_view_panes(&Scope::Channel(a))[0].id, card);
}
#[test]
fn worktree_projection_keeps_browser_ownership_and_durable_order() {
    let (mut app, _, _, first) = fixture();
    let path = PathBuf::from("/repo");
    app.sidebar_model.projects.push(SidebarProject::new(
        ProjectId::new(),
        RepositoryId::new(),
        "repo".into(),
        path.clone(),
        vec![],
        vec![],
    ));
    app.active_workspace = Some(path.clone());
    app.known_worktree_roots.insert(path.clone());
    let scope = Scope::Worktree {
        repository: path.clone(),
        path: path.clone(),
    };
    app.shared_views.initialized.insert(scope.clone());
    let mut second = first.clone();
    second.id = CardId::new();
    if let CardKind::Browser(browser) = &mut second.kind {
        browser.id = BrowserId::new();
    }
    let mut view = View::empty(scope.clone());
    view.panes = vec![first.clone(), second.clone()];
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
    assert_eq!(
        app.local_view_panes(&scope)
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
    view.panes.reverse();
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert_eq!(
        app.local_view_panes(&scope)
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>(),
        vec![second.id, first.id]
    );
    drop(app.select_worktree(path.clone(), false));
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        vec![second.id, first.id]
    );
    assert_eq!(
        app.card_worktree(app.card_state(first.id).unwrap()),
        Some(path.as_path())
    );
    for id in [first.id, second.id] {
        app.workspace_model.views[0]
            .apply(&Change::Remove { id })
            .unwrap();
    }
    drop(app.reconcile_shared_views());
    assert!(!app.card_state(first.id).unwrap().existence.is_open());
}

fn attention_terminal_source() -> TerminalSession {
    let mut session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    session.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("attention-owner"),
        boot_id: crate::model::BootId::new("attention-boot"),
        host_incarnation: crate::model::HostIncarnation::new("attention-host"),
    };
    session
}

/// A terminal's reported title is published with its deferred save, not
/// with each view acknowledgment. Until then a view edit from another
/// window, carrying the older shared title, must not take it back.
#[test]
fn a_reported_title_awaiting_its_save_survives_shared_view_edits() {
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    drop(app.insert_terminal_card(attention_terminal_source(), None));
    let card = app.workspace.focused().unwrap();
    let shared = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
    let mut view = View::empty(channel);
    view.panes.push(shared.clone());
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
    app.shared_views.pending.clear();
    let publishes_title = |app: &Cagents| {
        app.shared_views.pending.iter().any(|(_, scope, change)| {
            *scope == Scope::Channel(channel)
                && matches!(change, Change::Update { pane, .. } if pane.id == card && pane.title == "build done")
        })
    };

    assert!(app.apply_reported_title(card, "build done".into()));
    drop(app.save_reported_titles_soon(card));
    let mut wider = shared.clone();
    wider.width_fraction = 0.75;
    view.apply(&Change::Update {
        before: shared,
        pane: wider,
    })
    .unwrap();
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    let state = app.card_state(card).unwrap();
    assert_eq!(state.title, "build done");
    assert_eq!(
        state.width_fraction, 0.75,
        "the other window's edit applies"
    );
    assert_eq!(
        app.workspace.card(card).unwrap().accessible_name,
        "build done",
        "the pane keeps its newer name"
    );
    assert!(
        !publishes_title(&app),
        "an acknowledgment does not publish it"
    );
    assert!(app.reported_title_save.cards.contains(&card));

    drop(app.update(Message::ReportedTitleSaveDue));
    assert!(publishes_title(&app), "its save publishes it");
    assert!(app.reported_title_save.cards.is_empty());
    assert_eq!(app.card_state(card).unwrap().title, "build done");
}

/// Every save other than reconciliation's own publishes held titles, so
/// exiting, which saves, never leaves one unpublished.
#[test]
fn a_save_publishes_held_reported_titles() {
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    drop(app.insert_terminal_card(attention_terminal_source(), None));
    let card = app.workspace.focused().unwrap();
    let shared = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
    let mut view = View::empty(channel);
    view.panes.push(shared);
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    app.shared_views.pending.clear();

    assert!(app.apply_reported_title(card, "build done".into()));
    drop(app.save_reported_titles_soon(card));
    app.persist();
    assert!(app.reported_title_save.cards.is_empty());
    assert!(app.shared_views.pending.iter().any(|(_, _, change)| {
        matches!(change, Change::Update { pane, .. } if pane.id == card && pane.title == "build done")
    }));
}

/// A name given in another window replaces a held reported title, which
/// must then not be published back over that window's rename.
#[test]
fn a_shared_rename_releases_a_held_reported_title() {
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    drop(app.insert_terminal_card(attention_terminal_source(), None));
    let card = app.workspace.focused().unwrap();
    let shared = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
    let mut view = View::empty(channel);
    view.panes.push(shared.clone());
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
    app.shared_views.pending.clear();

    assert!(app.apply_reported_title(card, "build done".into()));
    drop(app.save_reported_titles_soon(card));
    let mut renamed = shared.clone();
    renamed.user_title = Some("Release".into());
    renamed.title = "Release".into();
    view.apply(&Change::Update {
        before: shared,
        pane: renamed,
    })
    .unwrap();
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());

    assert!(app.reported_title_save.cards.is_empty());
    assert_eq!(app.card_state(card).unwrap().title, "Release");
    assert_eq!(app.workspace.card(card).unwrap().accessible_name, "Release");
    app.shared_views.pending.clear();
    drop(app.update(Message::ReportedTitleSaveDue));
    assert!(
        !app.shared_views.pending.iter().any(|(_, _, change)| {
            matches!(change, Change::Update { pane, .. } if pane.title == "build done")
        }),
        "the replaced reported title is not published"
    );
}

/// A pane moving between views in the save that captures it is still in
/// its source's baseline. The move carries no title, so the held title
/// must not enter the destination's baseline unpublished.
#[test]
fn a_held_reported_title_stays_held_for_a_pane_moving_between_views() {
    let (mut app, source, destination, mut pane) = fixture();
    pane.title = "shared title".into();
    app.shared_views
        .baseline
        .insert(Scope::Channel(source), vec![pane.clone()]);
    let mut local = pane.clone();
    local.title = "held title".into();
    app.reported_title_save.cards.insert(pane.id);
    app.hold_reported_title(&Scope::Channel(destination), &mut local);
    assert_eq!(local.title, "shared title");

    app.reported_title_save.cards.clear();
    local.title = "held title".into();
    app.hold_reported_title(&Scope::Channel(destination), &mut local);
    assert_eq!(local.title, "held title", "a published title is not held");
}

#[test]
fn attention_passive_shared_removal_forgets_closed_unread_without_retaining_a_ghost() {
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    drop(app.insert_terminal_card(attention_terminal_source(), None));
    let closed = app.workspace.focused().unwrap();
    drop(app.insert_terminal_card(attention_terminal_source(), None));
    let other = app.workspace.focused().unwrap();
    let retained = Pane::from_card(app.card_state(other).unwrap(), None).unwrap();
    let project = ProjectId::new();
    let worktree = crate::model::WorktreeId::new();
    app.sidebar_state
        .apply(&SidebarAction::SetFilter(sidebar::SidebarFilter::Attention));
    app.sidebar_state
        .observe_agent_update(closed, project, worktree);
    app.sidebar_state.observe_card_update(other);
    app.sidebar_state.set_agent_activity(
        closed,
        project,
        worktree,
        crate::agent::Activity {
            provider: crate::agent::Provider::Codex,
            state: crate::agent::ActivityState::Waiting,
        },
    );
    app.shared_views.pending.clear();
    let mut view = View::empty(channel);
    view.panes.push(retained);
    view.retired.push(closed);
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.card_state(closed).unwrap().existence.is_open());
    assert!(app.card_state(other).unwrap().existence.is_open());
    assert!(!app.sidebar_state.has_card_update(closed));
    assert!(!app.sidebar_state.has_agent_update(project, worktree));
    assert!(app.sidebar_state.has_card_update(other));
}

#[test]
fn attention_passive_shared_close_forgets_surface_without_acknowledging_unseen_notice() {
    let (mut app, channel, _, _) = fixture();
    app.refresh_ui.focused = false;
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    let session = attention_terminal_source();
    drop(app.insert_terminal_card(session.clone(), None));
    let card = app.workspace.focused().unwrap();
    let id = uuid::Uuid::new_v4();
    let snapshot = crate::notifications::Snapshot {
        diagnostic: None,
        notices: vec![crate::notifications::Notice {
            id,
            delivered: true,
            acknowledged: false,
            window: None,
            event: crate::notifications::Event {
                content: None,
                target: crate::notifications::Target::Session(session.id),
                source_session: Some(session.id),
                provider: Some(crate::agent::Provider::Codex),
                directory: Some(session.launch.working_directory.clone()),
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: "unseen-before-passive-close".into(),
                    kind: crate::notifications::Kind::Succeeded,
                },
            },
        }],
    };
    drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot.clone())));
    assert!(app.sidebar_state.has_card_update(card));
    assert!(!app.notification_ui.acknowledged_for_test(id));
    app.shared_views.pending.clear();
    let mut view = View::empty(channel);
    view.retired.push(card);
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(!app.sidebar_state.has_card_update(card));
    assert!(
        !app.notification_ui.acknowledged_for_test(id),
        "surface retirement is not evidence the user read its content"
    );
    assert!(!app.notification_ui.snapshot.notices[0].acknowledged);
    // The durable journal may keep the unseen notice. A later broker
    // snapshot cannot recreate a live sidebar entry for its closed pane.
    drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot)));
    assert!(!app.sidebar_state.has_card_update(card));
    assert!(!app.notification_ui.acknowledged_for_test(id));
}

#[test]
fn same_card_session_replacement_retires_old_attention_without_reading_its_notice() {
    use crate::agent::{Activity, ActivityState, Provider};

    let (mut app, channel, _, mut original) = fixture();
    app.refresh_ui.focused = false;
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    let old_session = attention_terminal_source();
    original.kind = CardKind::Terminal(old_session.clone());
    let mut unrelated = original.clone();
    unrelated.id = CardId::new();
    unrelated.kind = CardKind::Terminal(attention_terminal_source());
    let mut view = View::empty(channel);
    view.panes = vec![original.clone(), unrelated.clone()];
    app.workspace_model.views = vec![view.clone()];
    drop(app.project_shared_view(&view, &HashSet::from([original.id, unrelated.id])));
    assert!(app.terminal_start_in_flight(original.id));
    assert!(matches!(
        app.cards.get(&original.id),
        Some(CardRuntime::Loading)
    ));

    let notice = uuid::Uuid::new_v4();
    let snapshot = crate::notifications::Snapshot {
        diagnostic: None,
        notices: vec![crate::notifications::Notice {
            id: notice,
            delivered: true,
            acknowledged: false,
            window: None,
            event: crate::notifications::Event {
                content: None,
                target: crate::notifications::Target::Session(old_session.id),
                source_session: Some(old_session.id),
                provider: Some(Provider::Codex),
                directory: Some(old_session.launch.working_directory.clone()),
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: "unseen-before-same-card-replacement".into(),
                    kind: crate::notifications::Kind::Succeeded,
                },
            },
        }],
    };
    drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot.clone())));
    let waiting = Activity {
        provider: Provider::Codex,
        state: ActivityState::Waiting,
    };
    app.observe_agent_activity_for_card(original.id, Some(waiting));
    app.observe_agent_update_for_card(unrelated.id);
    assert_eq!(app.sidebar_state.card_activity(original.id), Some(waiting));
    assert!(app.sidebar_state.has_card_update(original.id));
    assert!(app.sidebar_state.has_card_update(unrelated.id));
    app.shared_views.pending.clear();

    // A genuine presentation-only view update must preserve the source's
    // existing attention while the attachment task is still outstanding.
    let mut metadata = original.clone();
    metadata.title = "same terminal, renamed".into();
    view.apply(&Change::Update {
        before: original.clone(),
        pane: metadata.clone(),
    })
    .unwrap();
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
    assert_eq!(app.sidebar_state.card_activity(original.id), Some(waiting));
    assert!(app.sidebar_state.has_card_update(original.id));

    // The same pane ID may subsequently name an entirely new terminal.
    // Its real Loading/error projection must not borrow its predecessor's
    // activity or unread state, even before the new start can settle.
    let replacement_session = attention_terminal_source();
    assert_ne!(replacement_session.id, old_session.id);
    let mut replacement = metadata.clone();
    replacement.kind = CardKind::Terminal(replacement_session.clone());
    view.apply(&Change::Update {
        before: metadata,
        pane: replacement.clone(),
    })
    .unwrap();
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert_eq!(app.card_state(original.id).unwrap().kind, replacement.kind);
    assert!(matches!(
        app.cards.get(&original.id),
        Some(CardRuntime::Loading)
    ));
    assert!(app.terminal_start_in_flight(original.id));
    assert_eq!(app.sidebar_state.card_activity(original.id), None);
    assert!(!app.sidebar_state.has_card_update(original.id));
    assert!(app.sidebar_state.has_card_update(unrelated.id));
    assert!(!app.notification_ui.acknowledged_for_test(notice));
    assert!(!app.notification_ui.snapshot.notices[0].acknowledged);

    // Neither a delayed old attachment failure nor its still-unread
    // journal snapshot can assign old-session news to the replacement.
    drop(app.update(Message::TerminalStarted {
        card: original.id,
        terminal: old_session.id,
        result: Err("old attachment failed after session replacement".into()),
    }));
    assert!(matches!(
        app.cards.get(&original.id),
        Some(CardRuntime::Loading)
    ));
    drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot)));
    assert_eq!(app.sidebar_state.card_activity(original.id), None);
    assert!(!app.sidebar_state.has_card_update(original.id));
    assert!(!app.notification_ui.acknowledged_for_test(notice));

    // Fresh source observations remain usable. This invokes the App's
    // observer contract; no fixture start task or real host is executed.
    app.observe_agent_activity_for_card(original.id, Some(waiting));
    assert_eq!(app.sidebar_state.card_activity(original.id), Some(waiting));
    assert!(app.sidebar_state.has_card_update(original.id));
    drop(app.update(Message::TerminalStarted {
        card: original.id,
        terminal: replacement_session.id,
        result: Err("replacement attachment unavailable".into()),
    }));
    assert!(matches!(
        app.cards.get(&original.id),
        Some(CardRuntime::Failed(_))
    ));
    assert_eq!(app.sidebar_state.card_activity(original.id), None);
    assert!(
        app.sidebar_state.has_card_update(original.id),
        "fresh unread is not read by a runtime failure"
    );
    assert!(app.sidebar_state.has_card_update(unrelated.id));
    assert!(!app.notification_ui.acknowledged_for_test(notice));
}

#[test]
fn attention_alias_adoption_preserves_unread_only_for_the_exact_complete_native_source() {
    for source in 0..5 {
        let (mut app, channel, _, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        drop(app.insert_terminal_card(attention_terminal_source(), None));
        let alias = app.workspace.focused().unwrap();
        app.chat_ui =
            chat_ui::State::with_drafts(HashMap::from([(alias, "retained draft".into())]));
        let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
        canonical.id = CardId::new();
        let CardKind::Terminal(current) = &mut canonical.kind else {
            unreachable!()
        };
        match source {
            0 => {}
            1 => {
                if let TerminalContinuity::Live {
                    host_incarnation, ..
                } = &mut current.continuity
                {
                    *host_incarnation = crate::model::HostIncarnation::new("replacement-owner");
                }
            }
            2 => {
                current.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: current.launch.clone(),
                    last_exit: None,
                }
            }
            3 => {
                if let TerminalContinuity::Live { boot_id, .. } = &mut current.continuity {
                    *boot_id = crate::model::BootId::new("");
                }
            }
            4 => {}
            _ => unreachable!(),
        }
        app.sidebar_state.observe_card_update(alias);
        app.shared_views.pending.clear();
        let mut view = View::empty(channel);
        view.panes.push(canonical.clone());
        let duplicate = if source == 4 {
            let mut duplicate = canonical.clone();
            duplicate.id = CardId::new();
            let id = duplicate.id;
            view.panes.push(duplicate);
            Some(id)
        } else {
            None
        };
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert!(!app.card_state(alias).unwrap().existence.is_open());
        assert!(app.card_state(canonical.id).unwrap().existence.is_open());
        assert!(!app.sidebar_state.has_card_update(alias));
        assert_eq!(
            app.sidebar_state.has_card_update(canonical.id),
            source == 0,
            "source case{source}"
        );
        if let Some(duplicate) = duplicate {
            assert!(
                !app.sidebar_state.has_card_update(duplicate),
                "ambiguous ownership is not first-row routing"
            );
        }
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&canonical.id)
                .map(String::as_str),
            Some("retained draft")
        );
    }
}

#[test]
fn attention_chat_alias_proof_does_not_follow_another_native_thread_or_unknown_identity() {
    let source = attention_terminal_source();
    let proof = source.continuity.live_proof().unwrap();
    let a = crate::chat::Session {
        id: source.id,
        proof: Some(proof),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            session_id: Some("original-thread".into()),
            recipe: source.launch.clone(),
        },
    };
    assert!(same_attention_update_source(
        &CardKind::Chat(a.clone()),
        &CardKind::Chat(a.clone())
    ));
    let mut b = a.clone();
    b.launch.session_id = Some("successor-thread".into());
    assert!(!same_attention_update_source(
        &CardKind::Chat(a.clone()),
        &CardKind::Chat(b)
    ));
    let mut unknown = a.clone();
    unknown.launch.session_id = None;
    assert!(!same_attention_update_source(
        &CardKind::Chat(unknown.clone()),
        &CardKind::Chat(unknown)
    ));
    let mut missing = a.clone();
    missing.proof = None;
    assert!(!same_attention_update_source(
        &CardKind::Chat(missing.clone()),
        &CardKind::Chat(missing)
    ));
}

#[test]
fn repository_refresh_adopts_cross_repository_alias_without_losing_its_draft() {
    let (mut app, source, destination, _) = fixture();
    let source_project = app.durable.channels[0].project_id;
    let destination_project = ProjectId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|channel| channel.id == destination)
        .unwrap()
        .project_id = destination_project;
    let source_repository = PathBuf::from("/repo/.git");
    let destination_repository = PathBuf::from("/other/.git");
    for (id, repository) in [
        (source_project, source_repository),
        (destination_project, destination_repository.clone()),
    ] {
        app.sidebar_model.projects.push(SidebarProject::new(
            id,
            RepositoryId::new(),
            "repo".into(),
            repository,
            vec![],
            vec![],
        ));
    }
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(source);
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, None));
    let alias = app.workspace.focused().unwrap();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
        alias,
        "saved cross-repository draft  ".into(),
    )]));
    let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
    canonical.id = CardId::new();
    app.shared_views.pending.clear();
    let mut view = View::empty(destination);
    view.panes.push(canonical.clone());
    app.workspace_model.views = vec![View::empty(source), view];
    app.shared_views.topology_build_count.set(0);
    drop(app.reconcile_shared_views_for_repository(&destination_repository, &HashSet::new()));
    assert_eq!(app.shared_views.topology_build_count.get(), 1);
    assert!(!app.card_state(alias).unwrap().existence.is_open());
    assert!(app.card_state(canonical.id).unwrap().existence.is_open());
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards
            .iter()
            .all(|id| *id != alias)
    );
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == destination)
            .unwrap()
            .cards
            .contains(&canonical.id)
    );
    assert!(
        app.shared_views.baseline[&Scope::Channel(source)]
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
        Some("saved cross-repository draft  ")
    );
}

#[test]
fn terminal_inventory_alias_is_adopted_before_scope_removal_and_keeps_draft() {
    let (mut app, a, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session.clone(), None));
    let alias = app.workspace.focused().unwrap();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(alias, "saved local draft".into())]));
    let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
    canonical.id = CardId::new();
    app.shared_views.pending.clear();
    let mut view = View::empty(a);
    view.panes.push(canonical.clone());
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.card_state(alias).unwrap().existence.is_open());
    assert!(app.card_state(canonical.id).unwrap().existence.is_open());
    assert_eq!(
        app.shared_views.adoptions,
        vec![crate::persistence::TerminalPaneAdoption {
            alias,
            canonical: canonical.id,
            terminal: session.id
        }]
    );
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&canonical.id)
            .map(String::as_str),
        Some("saved local draft")
    );
}

#[test]
fn blocked_view_does_not_partially_retire_terminal_aliases() {
    let (mut app, a, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let mut panes = Vec::new();
    let mut aliases = Vec::new();
    for _ in 0..2 {
        let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
        drop(app.insert_terminal_card(session, None));
        let alias = app.workspace.focused().unwrap();
        let mut canonical = Pane::from_card(app.card_state(alias).unwrap(), None).unwrap();
        canonical.id = CardId::new();
        aliases.push(alias);
        panes.push(canonical);
    }
    // A draft already held by the canonical identity prevents adoption of
    // the second alias. Neither alias may retire before the view projects.
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([
        (aliases[0], "first draft".into()),
        (panes[1].id, "canonical draft".into()),
    ]));
    app.shared_views.pending.clear();
    let mut view = View::empty(a);
    view.panes = panes;
    app.workspace_model.views = vec![view];
    app.shared_views.topology_build_count.set(0);
    drop(app.reconcile_shared_views());
    assert_eq!(app.shared_views.topology_build_count.get(), 1);
    assert!(app.shared_views.blocked.contains(&Scope::Channel(a)));
    assert!(app.shared_views.adoptions.is_empty());
    for alias in &aliases {
        assert!(app.card_state(*alias).unwrap().existence.is_open());
    }
    assert!(
        app.persistent_workspace()
            .chat_drafts
            .contains_key(&aliases[0])
    );
    assert!(
        !app.persistent_workspace()
            .chat_drafts
            .contains_key(&aliases[1])
    );
    assert!(app.shared_chat_has_local_work(app.workspace_model.views[0].panes[1].id));
}

#[test]
fn retry_pending_terminal_preserves_the_reserved_identity() {
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    let source = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(source.clone(), None));
    let card = app.workspace.focused().unwrap();
    app.terminal_starts.lock().unwrap().clear();
    Arc::make_mut(&mut app.durable)
        .pending_terminal_creations
        .remove(&card);
    app.shared_views.attached.insert(card);
    app.shared_views.pending.clear();
    let mut pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
    pane.pending_terminal_start = true;
    let mut view = View::empty(channel);
    view.panes.push(pane);
    app.workspace_model.views = vec![view];
    app.cards
        .insert(card, CardRuntime::Failed("completion save refused".into()));

    let mut hosted = source.clone();
    hosted.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("pending-fixture"),
        boot_id: crate::model::BootId::new("pending-fixture"),
        host_incarnation: crate::model::HostIncarnation::new("pending-fixture"),
    };
    // Reconnecting inventory is evidence that the worker exists, not
    // permission to publish its source over the outstanding reservation.
    drop(app.observe_hosted_terminals(vec![hosted]));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(source.clone())
    );
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    assert!(app.terminal_starts.lock().unwrap().is_empty());

    drop(app.retry_card(card));

    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(source)
    );
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert!(app.terminal_starts.lock().unwrap().contains_key(&card));
}

#[test]
fn restored_pending_terminal_waits_for_registry_then_rehydrates() {
    let (mut app, a, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session.clone(), None));
    let id = app.workspace.focused().unwrap();
    app.terminal_starts.lock().unwrap().clear();
    app.shared_views.attached.insert(id);
    app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
    drop(app.start_terminal(id, session, true, false));
    assert!(app.terminal_starts.lock().unwrap().is_empty());
    app.cards.insert(id, CardRuntime::Loading);
    app.set_residency(id, Residency::Hydrating { epoch: 1 });
    app.lifecycle.register(
        id,
        Exposure::Visible,
        Residency::Hydrating { epoch: 1 },
        Instant::now(),
    );
    let mut pane = Pane::from_card(app.card_state(id).unwrap(), None).unwrap();
    pane.pending_terminal_start = true;
    let mut view = View::empty(a);
    view.panes.push(pane);
    app.shared_views.pending.clear();
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(app.terminal_starts.lock().unwrap().contains_key(&id));
}

/// A window restored with a terminal nobody has opened, while its registry
/// is still connecting.
fn restored_unopened_terminal() -> (Cagents, CardId, uuid::Uuid) {
    let (mut app, _, _, _) = fixture();
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, None));
    let id = app.workspace.focused().unwrap();
    let identity = app.workspace_model.identity;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    app.terminal_starts.lock().unwrap().clear();
    app.cards.insert(id, CardRuntime::Loading);
    app.set_exposure(id, Exposure::Offscreen);
    app.set_residency(id, Residency::Hibernated);
    app.lifecycle.register(
        id,
        Exposure::Offscreen,
        Residency::Hibernated,
        Instant::now(),
    );
    (app, id, identity)
}

/// #2104: a window restored while its registry is still connecting holds
/// every saved terminal it may share. Projecting a view admits its own held
/// terminals again once the registry answers; a terminal the registry lists
/// in no view has no projection, so it must be released too. It restores
/// as this window's own saved terminal rather than spinning forever or
/// waiting for another window to host it.
#[test]
fn restored_window_local_terminal_held_for_registry_restores_as_saved_once_it_answers() {
    let (mut app, _, _, _) = fixture();
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, None));
    let id = app.workspace.focused().unwrap();
    assert!(!app.durable.pending_terminal_creations.contains(&id));
    // Restart: the saved window had joined a registry, so its terminal
    // may be shared, and the new process has not heard from one yet.
    let identity = app.workspace_model.identity;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    app.terminal_starts.lock().unwrap().clear();
    app.cards.insert(id, CardRuntime::Loading);
    app.set_residency(id, Residency::Hibernated);
    app.lifecycle
        .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    assert!(
        matches!(
            app.lifecycle.state(id),
            Some((_, Residency::Hydrating { .. }))
        ),
        "the lifecycle counts the held terminal as hydrating"
    );
    assert!(
        app.terminal_starts.lock().unwrap().is_empty(),
        "a possibly shared terminal waits for the registry"
    );

    let model = crate::workspace_model::WorkspaceModel {
        identity,
        sequence: 1,
        ..crate::workspace_model::WorkspaceModel::default()
    };
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::Connected(model),
    ));
    assert!(
        !app.shared_views.attached.contains(&id),
        "a terminal the registry lists in no view is this window's own"
    );
    assert!(
        app.saved_terminal_current(id).is_some(),
        "it restores as a saved terminal, as an unshared window restores it"
    );
    // The one start in flight is the saved terminal's metadata-only
    // inspection, which attaches only an exact existing terminal.
    assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
    // Neither the lifecycle nor a later reconciliation starts the saved
    // command alongside it.
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    drop(app.reconcile_shared_views());
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    assert_eq!(
        app.terminal_starts.lock().unwrap().get(&id),
        Some(&1),
        "a saved recipe is not run without the user asking"
    );
    assert!(app.saved_terminal_current(id).is_some());
    assert!(!app.shared_views.attached.contains(&id));
}

/// A daemon that answers without a registry answers just the same: a held
/// terminal must not keep waiting for a registry it has been told is not
/// coming.
#[test]
fn restored_terminal_held_for_registry_restores_as_saved_once_it_is_unavailable() {
    let (mut app, _, _, _) = fixture();
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, None));
    let id = app.workspace.focused().unwrap();
    let identity = app.workspace_model.identity;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    app.terminal_starts.lock().unwrap().clear();
    app.cards.insert(id, CardRuntime::Loading);
    app.set_residency(id, Residency::Hibernated);
    app.lifecycle
        .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    assert!(
        app.terminal_starts.lock().unwrap().is_empty(),
        "a possibly shared terminal waits for the registry"
    );

    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::Unavailable(
            "daemon rejected the request (Internal): registry is unavailable".into(),
        ),
    ));
    assert!(app.saved_terminal_current(id).is_some());
    assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
}

/// A terminal nobody has opened is held just the same, and the gate waits
/// for it. It must not be reported as lost because no view lists it.
#[test]
fn unopened_terminal_no_view_lists_restores_as_saved_once_the_registry_answers() {
    let (mut app, id, identity) = restored_unopened_terminal();
    let model = crate::workspace_model::WorkspaceModel {
        identity,
        sequence: 1,
        ..crate::workspace_model::WorkspaceModel::default()
    };
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::Connected(model),
    ));
    assert!(!app.shared_views.attached.contains(&id));
    assert!(app.saved_terminal_current(id).is_some());
    assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
    assert!(matches!(
        app.lifecycle.state(id),
        Some((_, Residency::Hibernated))
    ));
}

/// Opening a terminal whose daemon has no registry must not run its saved
/// command: nothing has said no other window hosts it.
#[test]
fn unopened_terminal_never_runs_its_recipe_when_the_registry_is_unavailable() {
    let (mut app, id, _) = restored_unopened_terminal();
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::Unavailable(
            "daemon rejected the request (Internal): registry is unavailable".into(),
        ),
    ));
    assert!(app.saved_terminal_current(id).is_some());
    assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
    let CardKind::Terminal(session) = app.card_state(id).unwrap().kind.clone() else {
        panic!("a terminal card");
    };
    drop(app.start_terminal(id, session, true, false));
    assert_eq!(
        app.terminal_starts.lock().unwrap().get(&id),
        Some(&1),
        "opening it does not start anything beyond the inspection"
    );
}

/// A terminal that cannot restore as saved stays attach-only while the
/// daemon has no registry: nothing has said no other window hosts it.
#[test]
fn terminal_that_cannot_restore_as_saved_only_attaches_when_the_registry_is_unavailable() {
    let (mut app, id, _) = restored_unopened_terminal();
    let CardKind::Terminal(session) = &mut app.card_state_mut(id).unwrap().kind else {
        panic!("a terminal card");
    };
    session.continuity = crate::model::TerminalContinuity::Failed {
        typed_error: crate::model::RuntimeError {
            code: "failed".into(),
            message: "failed".into(),
        },
        retryable: false,
    };
    let session = session.clone();
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::Unavailable(
            "daemon rejected the request (Internal): registry is unavailable".into(),
        ),
    ));
    assert!(app.saved_terminal_current(id).is_none());
    assert!(app.shared_views.attached.contains(&id));
    assert!(app.terminal_start_held(id, false));
    drop(app.start_terminal(id, session, true, false));
    assert!(
        app.terminal_starts
            .lock()
            .unwrap()
            .get(&id)
            .is_none_or(|starts| *starts <= 1),
        "opening it adds no start of its own"
    );
}

/// A terminal restored as this window's own saved terminal can be listed
/// in a view later, by this window's own seed or once its folder is
/// opened as a project. Listed, it is shared and may only attach, so it
/// must leave the saved presentation and start on the attach-only path
/// rather than keep loading with nothing left to start it.
#[test]
fn saved_terminal_listed_in_a_view_later_hydrates_as_shared() {
    // The registry knows the channel, so its view can be projected here.
    let (mut app, a, _, _) = channel_move_fixture();
    let channels = app.workspace_model.channels.clone();
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, None));
    let id = app.workspace.focused().unwrap();
    // Saved before the window last closed, its creation long settled.
    Arc::make_mut(&mut app.durable)
        .pending_terminal_creations
        .remove(&id);
    let identity = app.workspace_model.identity;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    app.terminal_starts.lock().unwrap().clear();
    app.cards.insert(id, CardRuntime::Loading);
    app.set_residency(id, Residency::Hibernated);
    app.lifecycle
        .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    let model = crate::workspace_model::WorkspaceModel {
        identity,
        sequence: 1,
        channels,
        ..crate::workspace_model::WorkspaceModel::default()
    };
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::Connected(model),
    ));
    assert!(app.saved_terminal_current(id).is_some());
    assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));

    let mut view = View::empty(a);
    view.panes
        .push(Pane::from_card(app.card_state(id).unwrap(), None).unwrap());
    app.workspace_model.views = vec![view.clone()];
    drop(app.project_shared_view(&view, &HashSet::from([id])));
    assert!(app.shared_views.attached.contains(&id));
    assert!(
        !app.saved_terminals.entries.contains_key(&id),
        "a listed terminal is no longer presented as this window's saved terminal"
    );
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    assert_eq!(
        app.terminal_starts.lock().unwrap().get(&id),
        Some(&2),
        "the attach-only start runs beside the inspection it replaces"
    );
}

/// The shared counterpart: a held terminal the registry lists in a view
/// this window does not project (here the fixture's channels are absent
/// from the registry) is admitted to hydrate again and stays shared, so it
/// only attaches, since another window may host it.
#[test]
fn restored_shared_terminal_held_for_registry_attaches_once_it_answers() {
    let (mut app, a, _, _) = fixture();
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, None));
    let id = app.workspace.focused().unwrap();
    let identity = app.workspace_model.identity;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(identity);
    app.shared_views = State::for_workspace(&app.durable);
    app.shared_views.initialized.insert(Scope::Channel(a));
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    app.terminal_starts.lock().unwrap().clear();
    app.cards.insert(id, CardRuntime::Loading);
    app.set_residency(id, Residency::Hibernated);
    app.lifecycle
        .register(id, Exposure::Visible, Residency::Hibernated, Instant::now());
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    assert!(app.terminal_starts.lock().unwrap().is_empty());

    let mut view = View::empty(a);
    view.panes
        .push(Pane::from_card(app.card_state(id).unwrap(), None).unwrap());
    let model = crate::workspace_model::WorkspaceModel {
        identity,
        sequence: 1,
        views: vec![view],
        ..crate::workspace_model::WorkspaceModel::default()
    };
    drop(app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::Connected(model),
    ));
    drop(app.reconcile_lifecycle(&HashSet::from([id]), &HashSet::new(), &HashSet::new()));
    assert_eq!(app.terminal_starts.lock().unwrap().get(&id), Some(&1));
    assert!(app.shared_views.attached.contains(&id));
    assert!(app.saved_terminal_current(id).is_none());
}

#[test]
fn explicit_terminal_creation_is_durable_and_waits_for_reservation() {
    let (mut app, a, _, first) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let mut view = View::empty(a);
    view.panes.push(first.clone());
    app.workspace_model.views = vec![view.clone()];
    drop(app.project_shared_view(&view, &HashSet::from([first.id])));
    app.shared_views
        .baseline
        .insert(Scope::Channel(a), vec![first.clone()]);
    app.shared_views.pending.clear();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(app.insert_terminal_card(session, Some((0, 0))));
    let id = app.workspace.focused().unwrap();
    assert!(app.durable.pending_terminal_creations.contains(&id));
    assert!(app.terminal_starts.lock().unwrap().is_empty());
    assert!(std::iter::from_fn(||rx.try_recv().ok()).any(|edit| matches!(edit,
        ModelEdit::ChangeView { operation, change: Change::Insert { pane, before }, .. }
            if operation == id.as_uuid() && pane.id == id && pane.pending_terminal_start && before == Some(first.id))));
    let saved = app.persistent_workspace();
    assert!(saved.pending_terminal_creations.contains(&id));
    assert!(
        saved
            .cards
            .iter()
            .any(|card| card.id == id && card.existence.is_open())
    );
    // Restart before acknowledgment: the persisted intent still exports
    // the same pane and exact actor identity, and cannot spawn generically.
    app.shared_views = State::for_workspace(&saved);
    app.durable = saved;
    app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
    let pane = app
        .local_view_panes(&Scope::Channel(a))
        .into_iter()
        .find(|pane| pane.id == id)
        .unwrap();
    assert!(pane.pending_terminal_start);
    let CardKind::Terminal(session) = pane.kind.clone() else {
        panic!()
    };
    drop(app.start_terminal(id, session, true, false));
    assert!(app.terminal_starts.lock().unwrap().is_empty());
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.shared_views.initialized.insert(Scope::Channel(a));
    let mut view = View::empty(a);
    view.panes = vec![pane, first];
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.durable.pending_terminal_creations.contains(&id));
    assert!(app.terminal_starts.lock().unwrap().contains_key(&id));
}

#[test]
fn automatic_terminal_waits_for_authoritative_empty_view_reservation() {
    let (mut app, a, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(a);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::Terminal));
    let id = app.workspace.focused().unwrap();
    assert!(app.shared_views.defaults.contains_key(&id));
    assert!(app.terminal_starts.lock().unwrap().is_empty());
    assert!(std::iter::from_fn(||rx.try_recv().ok()).any(|edit|matches!(edit,ModelEdit::ChangeView {change:Change::InsertIfEmpty {pane},..} if pane.id==id)));
    let mut view = View::empty(a);
    view.panes
        .push(Pane::from_card(app.card_state(id).unwrap(), None).unwrap());
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert!(!app.shared_views.defaults.contains_key(&id));
    assert!(app.terminal_starts.lock().unwrap().contains_key(&id));
}
fn unpublished_default_chat() -> (
    Cagents,
    ChannelId,
    CardId,
    super::super::chat_ui::Target,
    TestModelRequests,
) {
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.workspace_model.views = vec![View::empty(channel)];
    let (sender, receiver) = test_model_channel();
    app.workspace_model_tx = sender;
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    let card = app.workspace.focused().unwrap();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("native default draft");
    };
    let target = runtime.target;
    assert!(app.chat_is_unused_default(card));
    assert!(app.local_view_panes(&Scope::Channel(channel)).is_empty());
    while receiver.try_recv().is_ok() {}
    (app, channel, card, target, receiver)
}

fn compose_default_chat(app: &mut Cagents, target: super::super::chat_ui::Target) {
    drop(app.handle_chat_ui(super::super::chat_ui::Action::Composer(
        target,
        iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(Arc::new(
            "first message".into(),
        ))),
    )));
}

#[test]
fn used_default_chat_reserves_one_insert_before_empty_view_projection() {
    let (mut app, channel, card, target, receiver) = unpublished_default_chat();
    // An untouched automatic composer remains local and ephemeral.
    drop(app.reconcile_shared_views());
    assert!(receiver.try_recv().is_err());
    compose_default_chat(&mut app, target);
    for _ in 0..3 {
        drop(app.reconcile_shared_views());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.workspace.card(card).is_some());
        assert!(!app.durable.local_only_cards.contains(&card));
        assert!(!app.shared_views.retained.contains(&card));
    }
    let edits = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    let [(operation, pane)]: [(uuid::Uuid, Pane); 1] = edits
        .iter()
        .filter_map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                change: Change::Insert { pane, .. },
                ..
            } if pane.id == card => Some((*operation, pane.clone())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let mut accepted = View::empty(channel);
    accepted.revision = 1;
    accepted.panes.push(pane.clone());
    app.workspace_model.views = vec![accepted];
    app.acknowledge_view_edit(Scope::Channel(channel), Some(operation));
    drop(app.reconcile_shared_views());
    assert!(receiver.try_recv().is_err());
    assert_eq!(app.local_view_cards(&Scope::Channel(channel)), vec![card]);
    let saved = app.persistent_workspace();
    let restored = State::for_workspace(&saved);
    assert!(restored.attached.contains(&card));
    assert!(!saved.local_only_cards.contains(&card));
    assert!(
        matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.is_unstarted())
    );
}

#[test]
fn an_unused_default_chat_gives_way_to_a_pane_another_window_adds() {
    let (mut app, channel, _, pane) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.workspace_model.views = vec![View::empty(channel)];
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    let card = app.workspace.focused().unwrap();
    drop(app.reconcile_shared_views());
    assert_eq!(app.local_view_cards(&Scope::Channel(channel)), vec![card]);
    // Never shared, but never going to be: an untouched automatic composer
    // is not a chat awaiting publication.
    let mut view = View::empty(channel);
    view.revision = 1;
    view.panes.push(pane.clone());
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert_eq!(
        app.local_view_cards(&Scope::Channel(channel)),
        vec![pane.id]
    );
}

fn first_start_save_completion(save: Task<Message>) -> super::super::chat_ui::Action {
    std::thread::spawn(move || {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut stream = cosmic::iced::runtime::task::into_stream(save).unwrap();
                while let Some(action) = tokio::time::timeout(Duration::from_secs(5), stream.next())
                    .await
                    .unwrap()
                {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        Message::ChatUi(
                            action @ super::super::chat_ui::Action::DraftStartCheckpointed(_, _, _),
                        ),
                    )) = action
                    {
                        return action;
                    }
                }
                panic!("checkpoint receipt");
            })
    })
    .join()
    .unwrap()
}

#[test]
fn first_start_empty_view_does_not_convert_creation_to_local_only() {
    let (mut app, channel, card, target, receiver) = unpublished_default_chat();
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("first-send-view"),
    )
    .unwrap();
    app.session = Some(writer);
    compose_default_chat(&mut app, target);
    // First publish A, then Send, then an empty snapshot while the save
    // receipt is waiting. This used to project A over the new runtime B.
    drop(app.reconcile_shared_views());
    let source = match &app.card_state(card).unwrap().kind {
        CardKind::Chat(session) => session.clone(),
        _ => unreachable!(),
    };
    let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
        target,
        crate::chat::PromptMode::Send,
    ));
    assert!(app.chat_start_pending(card));
    drop(app.reconcile_shared_views());
    assert!(!app.durable.local_only_cards.contains(&card));
    assert!(app.card_state(card).unwrap().existence.is_open());
    let completion = first_start_save_completion(save);
    assert!(matches!(
        &completion,
        super::super::chat_ui::Action::DraftStartCheckpointed(_, _, Ok(()))
    ));
    // Only the save Task ran. Drop the returned provider Task unpolled.
    let provider = app.handle_chat_ui(completion);
    let candidate = match &app.card_state(card).unwrap().kind {
        CardKind::Chat(session) => session.clone(),
        _ => unreachable!(),
    };
    assert_ne!(source.id, candidate.id);
    drop(app.reconcile_shared_views());
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.session.id == candidate.id)
    );
    assert!(!app.durable.local_only_cards.contains(&card));
    assert!(
        app.local_view_cards(&Scope::Channel(channel))
            .contains(&card)
    );
    let edits = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        edits
            .iter()
            .filter(|edit| matches!(edit,
        ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card))
            .count(),
        1
    );
    assert!(edits.iter().any(|edit| matches!(edit,
        ModelEdit::ChangeView { change: Change::Update { before, pane }, .. }
            if pane.id == card && before.kind == CardKind::Chat(source.clone()) && pane.kind == CardKind::Chat(candidate.clone()))));
    drop(provider);
}

#[test]
fn first_start_receipt_starts_a_refused_creation_locally_but_never_a_retired_one() {
    for case in ["refused", "refused-scope-gone", "retired"] {
        let (mut app, _, card, target, receiver) = unpublished_default_chat();
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("first-send-revoked"),
        )
        .unwrap();
        app.session = Some(writer);
        compose_default_chat(&mut app, target);
        drop(app.reconcile_shared_views());
        let operation = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|edit| match edit {
                ModelEdit::ChangeView {
                    operation,
                    change: Change::Insert { pane, .. },
                    ..
                } if pane.id == card => Some(operation),
                _ => None,
            })
            .unwrap();
        let draft = match &app.card_state(card).unwrap().kind {
            CardKind::Chat(session) => session.id,
            _ => unreachable!(),
        };
        let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
            target,
            crate::chat::PromptMode::Send,
        ));
        assert!(app.chat_start_pending(card));
        match case {
            "retired" => app.workspace_model.views[0].retired.push(card),
            _ => app.shared_views.refused(operation),
        }
        drop(app.reconcile_shared_views());
        assert!(app.durable.local_only_cards.contains(&card), "{case}");
        if case == "refused-scope-gone" {
            app.workspace_model.views.clear();
        }
        let completion = first_start_save_completion(save);
        assert!(matches!(
            &completion,
            super::super::chat_ui::Action::DraftStartCheckpointed(_, _, Ok(()))
        ));
        let start = app.handle_chat_ui(completion);
        let saved = app.persistent_workspace();
        assert!(saved.local_only_cards.contains(&card), "{case}");
        if case == "refused" {
            // Kept in this window, its one Send starts here and nowhere
            // else; the provider Task is dropped unpolled.
            assert!(start.units() > 0, "{case}");
            assert!(app.chat_start_pending(card), "{case}");
            assert!(
                matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.id != draft)
            );
        } else {
            // handle_chat_ui batches even an empty result; its stream may
            // exist without containing any work. No provider future is queued.
            assert_eq!(
                start.units(),
                0,
                "{case}: revoked creation must queue no provider work"
            );
            assert!(!app.chat_start_pending(card));
            assert!(
                matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.id == draft)
            );
            assert_eq!(
                saved.chat_drafts.get(&card).map(String::as_str),
                Some("first message")
            );
        }
        drop(start);
        assert!(
            !std::iter::from_fn(|| receiver.try_recv().ok()).any(|edit| matches!(edit,
                ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)),
            "{case}: a revoked or local creation must not be recreated"
        );
    }
}

#[test]
fn only_a_canonical_removal_reports_a_pane_closed_in_another_client() {
    for retired in [false, true] {
        let (mut app, channel, card, target, receiver) = unpublished_default_chat();
        compose_default_chat(&mut app, target);
        drop(app.reconcile_shared_views());
        let (operation, pane) = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|edit| match edit {
                ModelEdit::ChangeView {
                    operation,
                    change: Change::Insert { pane, .. },
                    ..
                } if pane.id == card => Some((operation, pane)),
                _ => None,
            })
            .unwrap();
        let mut accepted = View::empty(channel);
        accepted.revision = 1;
        accepted.panes.push(pane);
        app.workspace_model.views = vec![accepted];
        app.acknowledge_view_edit(Scope::Channel(channel), Some(operation));
        drop(app.reconcile_shared_views());
        assert!(app.status.is_none());
        // Another client removes the shared pane, or a later snapshot
        // simply lacks it, while this window still has its draft.
        let view = &mut app.workspace_model.views[0];
        view.panes.clear();
        view.revision += 1;
        if retired {
            view.retired.push(card);
        }
        for read in 0..3 {
            drop(app.reconcile_shared_views());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.durable.local_only_cards.contains(&card));
            if read == 0 {
                let notice = format!("{:?}", app.status.take().expect("one notice"));
                assert_eq!(
                    notice.contains("closed in another client"),
                    retired,
                    "{notice}"
                );
            } else {
                assert!(app.status.is_none(), "the notice is not repeated");
            }
        }
    }
}

#[test]
fn refused_draft_publication_is_kept_without_claiming_another_client_closed_it() {
    let (mut app, channel, card, target, receiver) = unpublished_default_chat();
    compose_default_chat(&mut app, target);
    drop(app.reconcile_shared_views());
    let operation = std::iter::from_fn(|| receiver.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                change: Change::Insert { pane, .. },
                ..
            } if pane.id == card => Some(operation),
            _ => None,
        })
        .unwrap();
    app.refuse_shared_view_edit(&Scope::Channel(channel), operation);
    assert!(app.durable.local_only_cards.contains(&card));
    // Said at the composer: the pane staying here is not the conversation
    // failing.
    assert!(
        super::super::chat_ui::chat_composer_note_for_test(&app, card)
            .is_some_and(|note| note.contains("could not be shared"))
    );
    assert_eq!(super::super::chat_ui::chat_error_for_test(&app, card), None);
    for _ in 0..3 {
        drop(app.reconcile_shared_views());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(app.status.is_none(), "nothing was closed in another client");
    }
    assert!(receiver.try_recv().is_err(), "a refusal is not republished");
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&card)
            .map(String::as_str),
        Some("first message")
    );
    // The kept draft still sends, in this window only.
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("refused-draft-send"),
    )
    .unwrap();
    app.session = Some(writer);
    let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
        target,
        crate::chat::PromptMode::Send,
    ));
    let start = app.handle_chat_ui(first_start_save_completion(save));
    assert!(start.units() > 0);
    assert!(app.chat_start_pending(card));
    drop(start);
    drop(app.reconcile_shared_views());
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.durable.local_only_cards.contains(&card));
    assert!(
        !std::iter::from_fn(|| receiver.try_recv().ok()).any(|edit| matches!(edit,
            ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card))
    );
}

#[test]
fn transferred_native_draft_can_complete_its_first_start_checkpoint() {
    let (mut app, channel, card, target, receiver) = unpublished_default_chat();
    compose_default_chat(&mut app, target);
    drop(app.reconcile_shared_views());
    let pane = app
        .local_view_panes(&Scope::Channel(channel))
        .into_iter()
        .find(|pane| pane.id == card)
        .unwrap();
    let source = app
        .durable
        .channels
        .iter()
        .find(|entry| entry.id != channel)
        .unwrap()
        .id;
    let mut retired_source = View::empty(source);
    retired_source.retired.push(card);
    app.workspace_model.views[0].panes.push(pane);
    app.workspace_model.views.push(retired_source);
    app.shared_views.pending.clear();
    drop(app.reconcile_shared_views());
    while receiver.try_recv().is_ok() {}
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("first-send-transferred"),
    )
    .unwrap();
    app.session = Some(writer);
    let save = app.handle_chat_ui(super::super::chat_ui::Action::Submit(
        target,
        crate::chat::PromptMode::Send,
    ));
    let completion = first_start_save_completion(save);
    assert!(matches!(
        &completion,
        super::super::chat_ui::Action::DraftStartCheckpointed(_, _, Ok(()))
    ));
    let start = app.handle_chat_ui(completion);
    assert!(app.chat_start_pending(card));
    assert!(start.units() > 0);
    drop(start);
    assert!(!app.durable.local_only_cards.contains(&card));
    assert_eq!(app.local_view_cards(&Scope::Channel(channel)), vec![card]);
}

#[test]
fn refused_default_chat_insert_does_not_republish_on_empty_reads() {
    let (mut app, _, card, target, receiver) = unpublished_default_chat();
    compose_default_chat(&mut app, target);
    drop(app.reconcile_shared_views());
    let operation = std::iter::from_fn(|| receiver.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                change: Change::Insert { pane, .. },
                ..
            } if pane.id == card => Some(operation),
            _ => None,
        })
        .unwrap();
    app.shared_views.refused(operation);
    for _ in 0..3 {
        drop(app.reconcile_shared_views());
    }
    assert!(app.durable.local_only_cards.contains(&card));
    assert!(
        receiver.try_recv().is_err(),
        "a refusal grants no creation retry"
    );
}

#[test]
fn retired_default_chat_does_not_gain_creation_authority() {
    let (mut app, _, card, target, receiver) = unpublished_default_chat();
    compose_default_chat(&mut app, target);
    app.workspace_model.views[0].retired.push(card);
    drop(app.reconcile_shared_views());
    assert!(app.durable.local_only_cards.contains(&card));
    assert!(receiver.try_recv().is_err());
}

/// Starts the window over on its saved state: its views are uninitialized
/// and it reconciles against the registry's. Returns the edits it sent.
fn restart_shared_views(app: &mut Cagents, scope: &Scope) -> Vec<ModelEdit> {
    app.shared_views.initialized.remove(scope);
    app.shared_views.baseline.remove(scope);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn seeded(edits: &[ModelEdit], scope: &Scope) -> bool {
    edits
        .iter()
        .any(|edit| matches!(edit, ModelEdit::SeedView { scope: seeded, .. } if seeded == scope))
}

#[test]
fn a_pane_name_the_registry_lacks_is_published_so_the_next_start_admits_its_view() {
    // A window that saved a pane name the registry's pane does not hold,
    // as one given before the registry existed or whose publication
    // failed, cannot admit its view and migrates it. The migration is
    // additive, so the registry keeps its unnamed pane.
    let (mut app, source, _, pane) = channel_move_fixture();
    app.active_workspace = Some("/repo".into());
    let scope = Scope::Channel(source);
    app.chrome_ui.user_titles.insert(pane.id, "Pinned".into());
    assert!(seeded(&restart_shared_views(&mut app, &scope), &scope));

    // Once the seed lands, the window publishes the name, which a person
    // gave and cannot have removed, and keeps it meanwhile.
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    app.acknowledge_view_edit(scope.clone(), None);
    drop(app.reconcile_shared_views());
    let operation = std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                scope: changed,
                operation,
                change:
                    Change::Update {
                        before,
                        pane: named,
                    },
                ..
            } if changed == scope
                && named.id == pane.id
                && before.user_title.is_none()
                && named.user_title.as_deref() == Some("Pinned") =>
            {
                Some(operation)
            }
            _ => None,
        })
        .expect("the window publishes its pane name");
    assert_eq!(
        app.chrome_ui.user_titles.get(&pane.id).map(String::as_str),
        Some("Pinned")
    );

    // The registry records it, so the next start admits the view.
    let view = &mut app.workspace_model.views[0];
    view.revision += 1;
    view.panes[0].user_title = Some("Pinned".into());
    app.acknowledge_view_edit(scope.clone(), Some(operation));
    drop(app.reconcile_shared_views());
    let edits = restart_shared_views(&mut app, &scope);
    assert!(app.shared_views.initialized.contains(&scope));
    assert!(!seeded(&edits, &scope), "{edits:?}");
    assert_eq!(
        app.chrome_ui.user_titles.get(&pane.id).map(String::as_str),
        Some("Pinned")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_pane_name_the_registry_changed_is_saved_so_the_next_start_admits_its_view() {
    use cosmic::iced::futures::StreamExt;
    // Another window renamed the pane while this one was closed. The
    // registry's name is newer, and the window takes it.
    let (mut app, source, _, pane) = channel_move_fixture();
    app.active_workspace = Some("/repo".into());
    let scope = Scope::Channel(source);
    app.workspace_model.views[0].panes[0].user_title = Some("Theirs".into());
    app.chrome_ui.user_titles.insert(pane.id, "Pinned".into());
    assert!(seeded(&restart_shared_views(&mut app, &scope), &scope));

    let directory = tempfile::tempdir().unwrap();
    let (session, _) = SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("pane-name-test"),
    )
    .unwrap();
    let store = session.operation_store();
    app.session = Some(session);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    app.acknowledge_view_edit(scope.clone(), None);
    let task = app.reconcile_shared_views();
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::ChangeView { change: Change::Update { before, pane: named }, .. }
                if before.user_title != named.user_title
        )),
        "the registry's newer name is not overwritten"
    );
    assert_eq!(
        app.chrome_ui.user_titles.get(&pane.id).map(String::as_str),
        Some("Theirs")
    );

    // Pane names live in the window's settings, not its workspace save.
    // Unsaved, the next start reads the old name back and migrates the
    // view again (#3032).
    let mut stream = iced::runtime::task::into_stream(task).expect("projection saves");
    let saved = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(Message::SettingsSaved {
                result,
                ..
            })) = action
            {
                return result;
            }
        }
        Err("projection did not save the window's settings".into())
    })
    .await
    .expect("the settings save finishes");
    saved.unwrap();
    let view: WindowView =
        serde_json::from_str(&store.load_window_view().unwrap().unwrap()).unwrap();
    assert_eq!(
        view.user_titles.get(&pane.id).map(String::as_str),
        Some("Theirs")
    );

    // The next start reads those settings and admits the view.
    app.chrome_ui = super::super::chrome_ui::State::default().with_user_titles(view.user_titles);
    let edits = restart_shared_views(&mut app, &scope);
    assert!(app.shared_views.initialized.contains(&scope));
    assert!(!seeded(&edits, &scope), "{edits:?}");
}

#[test]
fn edit_during_seed_is_published_before_projection() {
    let (mut app, a, _, pane) = fixture();
    app.shared_views.initialized.remove(&Scope::Channel(a));
    app.shared_views.seeding.insert(Scope::Channel(a));
    app.shared_views.baseline.insert(Scope::Channel(a), vec![]);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.project_shared_view(
        &View {
            scope: Scope::Channel(a),
            revision: 0,
            panes: vec![pane.clone()],
            retired: vec![],
        },
        &HashSet::from([pane.id]),
    ));
    app.acknowledge_view_edit(Scope::Channel(a), None);
    assert!(std::iter::from_fn(||rx.try_recv().ok()).any(|edit|matches!(edit, ModelEdit::ChangeView {change:Change::Insert {pane: inserted,..},..} if inserted.id==pane.id)));
}

#[test]
fn channel_move_waits_for_actual_terminal_start_completion() {
    let (mut app, source, destination, mut pane) = channel_move_fixture();
    pane.kind = moving_terminal_kind();
    let CardKind::Terminal(session) = &pane.kind else {
        unreachable!();
    };
    app.workspace_model
        .views
        .iter_mut()
        .flat_map(|view| &mut view.panes)
        .find(|current| current.id == pane.id)
        .unwrap()
        .kind = pane.kind.clone();
    // Projection registers the actual start before its worker Task runs.
    // Hold that Task and exercise the ordinary menu/direct/drop handlers.
    let attach = app.reconcile_shared_views();
    assert!(app.terminal_start_in_flight(pane.id));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, pane.id);
    drop(attach);
    drop(app.update(Message::TerminalStarted {
        card: pane.id,
        terminal: session.id,
        result: Err("fixture holds the real terminal attach".into()),
    }));
    assert!(!app.terminal_start_in_flight(pane.id));
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
        change: Change::Transfer { id, .. }, ..
    } if id == pane.id));
}

fn assert_lifecycle_refuses_moves(
    app: &mut Cagents,
    rx: &TestModelRequests,
    source: ChannelId,
    destination: ChannelId,
    card: CardId,
) {
    let before = app.workspace_model.clone();
    assert!(!app.pane_has_move_destination(card));
    assert!(!app.can_move_pane_to_channel(card, destination));
    assert!(app.pane_move_destinations(card).is_empty());
    drop(app.move_pane_to_channel(card, destination));
    drop(app.update(Message::MovePaneToChannel {
        card,
        channel: destination,
    }));
    drop(app.update(Message::PaneDropToChannel {
        generation: app.workspace_view_generation,
        card,
        channel: destination,
    }));
    drop(app.update_palette(super::super::palette_ui::Action::MovePane(card)));
    assert!(!app.palette.is_open());
    assert!(rx.try_recv().is_err());
    assert!(!app.pane_transfer_pending(card));
    assert_eq!(app.workspace_model, before);
    assert_eq!(app.durable.active_channel, Some(source));
    assert!(app.card_state(card).unwrap().existence.is_open());
}

#[test]
fn channel_move_waits_for_an_existing_workspace_close_handoff() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    // Hold the real close's focus task before it can commit removal.
    let close = app.begin_card_close(pane.id);
    assert!(
        app.workspace
            .pending_close()
            .is_some_and(|handoff| handoff.unmounting == pane.id)
    );
    assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, pane.id);
    drop(close);
    app.workspace.cancel_close(pane.id).unwrap();
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
        change: Change::Transfer { id, .. }, ..
    } if id == pane.id));
}

#[test]
fn channel_move_waits_for_pending_terminal_cleanup_and_explicit_dismissal() {
    let (mut app, source, destination, pane) =
        channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    // Begin the actual production close, but hold its worker task unpolled.
    let close = app.begin_card_close(pane.id);
    let intent = app.terminal_close.as_ref().unwrap().clone();
    assert!(intent.pending);
    assert!(!app.modal_is_open());
    assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, pane.id);
    drop(close);
    drop(app.terminal_close_finished(
        pane.id,
        intent.terminal,
        intent.proof,
        intent.generation,
        super::super::terminal_close::CloseOutcome::Unreachable("fixture held close".into()),
    ));
    assert!(!app.can_move_pane_to_channel(pane.id, destination));
    drop(app.cancel_terminal_close());
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
        change: Change::Transfer { id, .. }, ..
    } if id == pane.id));
}

#[test]
fn channel_move_stays_available_while_an_unrelated_terminal_is_closing() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let mut other = pane.clone();
    other.id = CardId::new();
    other.kind = moving_terminal_kind();
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .panes
        .push(other.clone());
    drop(app.reconcile_shared_views());
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let close = app.begin_card_close(other.id);
    assert!(app.terminal_close_pending(other.id));
    assert!(!app.modal_is_open());
    assert!(!app.can_move_pane_to_channel(other.id, destination));
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
        change: Change::Transfer { id, .. }, ..
    } if id == pane.id));
    assert!(rx.try_recv().is_err());
    assert_eq!(app.terminal_close.as_ref().unwrap().card, other.id);
    assert!(app.card_state(other.id).unwrap().existence.is_open());
    drop(close);
}

fn retained_chat_move_fixture() -> (Cagents, ChannelId, ChannelId, Pane) {
    use crate::chat;
    let (mut app, source, destination, mut pane) = channel_move_fixture();
    let CardKind::Terminal(terminal) = moving_terminal_kind() else {
        unreachable!()
    };
    let session = chat::Session {
        id: TerminalId::new(),
        proof: terminal.continuity.live_proof(),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: chat::NativeContinuity::Durable {
            provider_version: "fixture".into(),
        },
        launch: chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: terminal.launch,
            session_id: Some("retained-close-fixture".into()),
        },
    };
    let kind = CardKind::Chat(session);
    pane.kind = kind.clone();
    app.card_state_mut(pane.id).unwrap().kind = kind.clone();
    app.workspace_model
        .views
        .iter_mut()
        .flat_map(|view| &mut view.panes)
        .find(|candidate| candidate.id == pane.id)
        .unwrap()
        .kind = kind;
    // This is the real recovery path: the saved owner remains while its
    // unavailable surface waits for an exact native close receipt.
    app.cards.insert(
        pane.id,
        CardRuntime::Failed("fixture detached native surface".into()),
    );
    (app, source, destination, pane)
}

/// A closed chat whose removal its shared view refuses, judged against the
/// conversation still running, is kept by the view while that stops. It
/// closed, so it stays out of sight, nothing attaches to its conversation,
/// and no one is told to close it again. A refused stop brings it back on
/// what still runs, saying why on the pane rather than in a notice; once its
/// conversation has stopped it goes, as it was asked to.
#[test]
fn a_closed_chat_its_view_keeps_waits_for_its_conversation_to_stop() {
    for refused in [false, true] {
        let (mut app, _source, _destination, pane) = retained_chat_move_fixture();
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        drop(app.begin_chat_close(pane.id));
        assert!(!app.card_state(pane.id).unwrap().existence.is_open());
        let stopping = app
            .background_close_for_test(pane.id)
            .expect("the conversation stops in the background");
        let (operation, scope, change) = app
            .shared_views
            .pending
            .iter()
            .find(|(_, _, change)| {
                matches!(change, Change::RemoveSource { before, .. } if before.id == pane.id)
            })
            .cloned()
            .expect("its removal is published");
        while rx.try_recv().is_ok() {}

        drop(app.refuse_conflicting_view_change(
            scope,
            operation,
            change,
            crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT.into(),
        ));
        assert!(
            app.card_state(pane.id)
                .is_some_and(|state| state.existence.is_open()),
            "refused={refused}: the view keeps it"
        );
        assert!(app.holds_reopen(pane.id), "refused={refused}");
        assert!(
            !app.cards.contains_key(&pane.id) && !app.chat_start_pending(pane.id),
            "refused={refused}: nothing attaches to the conversation being stopped"
        );
        assert!(
            app.workspace.is_hidden(pane.id),
            "refused={refused}: it stays out of sight"
        );
        let told = app
            .status
            .iter()
            .chain(app.notices.recent().map(|entry| &entry.notice))
            .map(|notice| notice.message.to_string())
            .filter(|message| message == PANE_KEPT || message == PANE_KEPT_ELSEWHERE)
            .collect::<Vec<_>>();
        assert!(
            told.is_empty(),
            "refused={refused}: no one is told to close it again: {told:?}"
        );

        let result = if refused {
            Err("the retained provider is still running".to_owned())
        } else {
            Ok(())
        };
        drop(
            app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
                stopping, result,
            ))),
        );
        assert!(!app.holds_reopen(pane.id));
        if refused {
            assert!(
                app.chat_start_pending(pane.id),
                "it goes back to what still runs"
            );
            assert!(!app.workspace.is_hidden(pane.id), "in sight");
            assert!(
                !app.has_close_failure_notices(),
                "its pane says why, so no notice does"
            );
        } else {
            assert!(
                !app.card_state(pane.id)
                    .is_some_and(|state| state.existence.is_open()),
                "with nothing running, it goes"
            );
            assert!(!app.chat_start_pending(pane.id));
        }
    }
}

/// Closes the retained chat of [`retained_chat_move_fixture`] and has its
/// view refuse the removal, so the closed pane is back, out of sight, while
/// its conversation stops. Returns the app, the pane and the stop.
fn closed_chat_its_view_kept() -> (Cagents, Pane, super::super::chat_ui::Target) {
    closed_chat_its_view_kept_from(None, "")
}

/// [`closed_chat_its_view_kept`], for a chat working in `directory`, when
/// given, that had `unsent` written but not sent as it closed.
fn closed_chat_its_view_kept_from(
    directory: Option<&std::path::Path>,
    unsent: &str,
) -> (Cagents, Pane, super::super::chat_ui::Target) {
    let (mut app, _source, _destination, mut pane) = retained_chat_move_fixture();
    if let Some(directory) = directory {
        let retarget = |kind: &mut CardKind| {
            if let CardKind::Chat(session) = kind {
                session.launch.recipe.working_directory = directory.to_path_buf();
            }
        };
        retarget(&mut pane.kind);
        retarget(&mut app.card_state_mut(pane.id).unwrap().kind);
        app.workspace_model
            .views
            .iter_mut()
            .flat_map(|view| &mut view.panes)
            .filter(|candidate| candidate.id == pane.id)
            .for_each(|candidate| retarget(&mut candidate.kind));
    }
    if !unsent.is_empty() {
        super::super::chat_ui::leave_unsent_for_test(&mut app, pane.id, unsent);
    }
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.begin_chat_close(pane.id));
    let stopping = app
        .background_close_for_test(pane.id)
        .expect("the conversation stops in the background");
    let (operation, scope, change) = app
        .shared_views
        .pending
        .iter()
        .find(|(_, _, change)| {
            matches!(change, Change::RemoveSource { before, .. } if before.id == pane.id)
        })
        .cloned()
        .expect("its removal is published");
    while rx.try_recv().is_ok() {}
    drop(app.refuse_conflicting_view_change(
        scope,
        operation,
        change,
        crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT.into(),
    ));
    assert!(app.workspace.is_hidden(pane.id), "back, out of sight");
    (app, pane, stopping)
}

/// The next launch, from what `app` saved: a closed pane its view kept,
/// whose conversation was heard to stop, stays out of sight and goes, with
/// nothing asked again, rather than coming back as a chat.
/// Restored from its view, the pane shows its saved text until the view
/// admits it; it stays out of sight, its stop saved, until admission lets
/// its removal reach the view.
/// A pane its view fences is never admitted, so it goes by identity at once.
fn next_launch_takes_away(app: &mut Cagents, pane: &Pane) {
    for (from_view, fenced) in [(false, false), (true, false), (true, true)] {
        next_launch_takes_away_as(app, pane, from_view, fenced);
    }
}

fn next_launch_takes_away_as(app: &mut Cagents, pane: &Pane, from_view: bool, fenced: bool) {
    let mut next = crate::app::tests::context_inspector_test_app(false);
    next.durable = app.persistent_workspace();
    next.workspace = Cagents::rebuild_workspace(&next.durable);
    assert!(
        next.card_state(pane.id)
            .is_some_and(|state| state.existence.is_open()),
        "the view saved it"
    );
    let CardKind::Chat(session) = next.card_state(pane.id).unwrap().kind.clone() else {
        panic!("a chat pane");
    };
    assert!(
        next.awaits_stop(pane.id, &session),
        "it does not attach at launch"
    );
    if from_view {
        // As the launch restores a pane its view holds.
        next.await_shared_chat_admission(pane.id, session.clone());
        assert!(next.chat_awaiting_admission(pane.id));
        if fenced {
            next.shared_views.quarantined.insert(pane.id);
        }
    }
    drop(next.resume_stopping_chats());
    assert!(
        !next.workspace.shown().any(|card| card == pane.id),
        "from_view={from_view}: it is never in sight"
    );
    assert!(
        next.background_close_for_test(pane.id).is_none(),
        "from_view={from_view}: nothing is asked again of what stopped"
    );
    if from_view && !fenced {
        // Only an admitted pane's removal reaches its view: until then it
        // stays out of sight, its stop saved for a launch that comes first.
        assert!(
            next.card_state(pane.id)
                .is_some_and(|state| state.existence.is_open())
        );
        assert!(next.workspace.is_hidden(pane.id), "out of sight meanwhile");
        assert!(next.closes_out_of_sight(pane.id));
        assert!(!next.chat_start_pending(pane.id));
        assert!(
            !next.persistent_workspace().stopping_chats.is_empty(),
            "a launch before admission still takes it away"
        );
        let mut admitted = pane.clone();
        admitted.kind = CardKind::Chat(session.clone());
        let (task, copy) = next.admit_shared_chat(&admitted);
        drop(task);
        assert_eq!(copy, None);
    }
    assert!(
        !next
            .card_state(pane.id)
            .is_some_and(|state| state.existence.is_open()),
        "from_view={from_view} fenced={fenced}: it goes"
    );
    assert!(!next.chat_start_pending(pane.id));
    if fenced {
        assert!(
            next.shared_views.fenced_closes.contains(&pane.id),
            "its view hears that it closed, by identity"
        );
    }
    assert!(
        next.durable.stopping_chats.is_empty(),
        "from_view={from_view}: nothing is left for a later launch"
    );
    assert!(
        next.durable.pending_chat_admissions.is_empty(),
        "from_view={from_view}: nothing is left for its view to admit"
    );
    assert!(!next.has_close_failure_notices());
}

/// A window on its way out when the stop of a closed pane its view kept
/// succeeds goes before it can take the pane away again. The stop stays
/// saved, so the next launch asks again and takes the pane away then,
/// rather than restoring the closed chat from the view.
#[test]
fn a_kept_closed_pane_whose_stop_settles_at_exit_stays_saved() {
    let (mut app, pane, stopping) = closed_chat_its_view_kept();
    let CardKind::Chat(session) = app.card_state(pane.id).unwrap().kind.clone() else {
        panic!("a chat pane");
    };
    app.exit = Some(ApplicationExit {
        closing: None,
        servers_reaped: false,
        chats_closed: false,
    });
    drop(
        app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
            stopping,
            Ok(()),
        ))),
    );
    assert!(
        app.durable
            .stopping_chats
            .iter()
            .any(|stopping| stopping.session.id == session.id && stopping.refused.is_none()),
        "the next launch asks again"
    );
    assert!(app.exit.is_some_and(|exit| exit.chats_closed));
    next_launch_takes_away(&mut app, &pane);
}

/// A closed pane its view kept, whose conversation stops while another
/// strip is shown, stays out of sight and goes once its own strip is shown
/// again, with no error. Until then its stop stays saved, so a window that
/// closes first takes it away at the next launch instead.
#[test]
fn a_kept_closed_pane_whose_stop_settles_off_strip_goes_with_its_strip() {
    for visited in [true, false] {
        let (mut app, pane, stopping) = closed_chat_its_view_kept();
        let strip = Cagents::rebuild_workspace(&app.durable);
        app.workspace = Cagents::rebuild_workspace(&Default::default());
        app.status = None;
        drop(
            app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
                stopping,
                Ok(()),
            ))),
        );
        assert!(
            app.status.is_none(),
            "visited={visited}: nothing went wrong: {:?}",
            app.status
        );
        assert!(
            app.closes_out_of_sight(pane.id),
            "visited={visited}: no row offers it meanwhile"
        );
        let CardKind::Chat(session) = app.card_state(pane.id).unwrap().kind.clone() else {
            panic!("a chat pane");
        };
        assert!(
            !app.awaits_stop(pane.id, &session),
            "visited={visited}: this window heard it stop"
        );
        if !visited {
            next_launch_takes_away(&mut app, &pane);
            continue;
        }
        app.workspace = strip;
        app.rehide_closing_panes();
        assert!(app.workspace.is_hidden(pane.id), "out of sight");
        drop(app.remove_stopped_returns());
        assert!(
            !app.card_state(pane.id)
                .is_some_and(|state| state.existence.is_open()),
            "it goes with its strip"
        );
        assert!(!app.chat_start_pending(pane.id));
        assert!(
            app.durable.stopping_chats.is_empty(),
            "nothing is left for the next launch"
        );
        assert!(!app.closes_out_of_sight(pane.id));
    }
}

/// A closed pane its view kept, whose conversation stopped while another
/// strip was shown, stays when another window puts a different conversation
/// in it meanwhile: it comes back into sight and is not taken away with its
/// strip, nor is a stop saved for it.
#[test]
fn a_kept_closed_pane_another_window_reuses_stays() {
    let (mut app, pane, stopping) = closed_chat_its_view_kept();
    let strip = Cagents::rebuild_workspace(&app.durable);
    app.workspace = Cagents::rebuild_workspace(&Default::default());
    drop(
        app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
            stopping,
            Ok(()),
        ))),
    );
    assert!(app.closes_out_of_sight(pane.id));
    let CardKind::Chat(stopped) = app.card_state(pane.id).unwrap().kind.clone() else {
        panic!("a chat pane");
    };
    let mut other = stopped.clone();
    other.id = TerminalId::new();
    other.launch.session_id = Some("another-conversation".into());
    drop(app.attach_shared_chat(pane.id, other));
    assert!(!app.closes_out_of_sight(pane.id), "back in sight");
    assert!(
        app.durable.stopping_chats.is_empty(),
        "no stop is saved for a pane that stays"
    );
    app.workspace = strip;
    app.rehide_closing_panes();
    assert!(!app.workspace.is_hidden(pane.id));
    drop(app.remove_stopped_returns());
    assert!(
        app.card_state(pane.id)
            .is_some_and(|state| state.existence.is_open()),
        "it stays with its strip"
    );
}

/// Reopening the conversation of a closed pane its view kept, while the
/// conversation stops or once it has stopped with the pane's strip out of
/// sight, opens it in a new pane with what the closed one had not sent. The
/// pane out of sight is not where the conversation is, and is not open in
/// another window (#3620). A next launch takes the closed pane away without
/// holding the reopened one, and a refused stop leaves one pane on what
/// still runs.
#[test]
fn a_kept_closed_pane_does_not_keep_its_conversation_from_reopening() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Reopen {
        WhileStopping,
        WhileStoppingThenRefused,
        AfterStopping,
        AfterStoppingThenRelaunch,
    }
    use Reopen::*;
    for case in [
        WhileStopping,
        WhileStoppingThenRefused,
        AfterStopping,
        AfterStoppingThenRelaunch,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, pane, stopping) =
            closed_chat_its_view_kept_from(Some(directory.path()), "still writing");
        let CardKind::Chat(session) = app.card_state(pane.id).unwrap().kind.clone() else {
            panic!("a chat pane");
        };
        let strip = Cagents::rebuild_workspace(&app.durable);
        let retire = |app: &mut Cagents, result: Result<(), String>| {
            drop(
                app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
                    stopping, result,
                ))),
            )
        };
        let settled = matches!(case, AfterStopping | AfterStoppingThenRelaunch);
        if settled {
            app.workspace = Cagents::rebuild_workspace(&Default::default());
            retire(&mut app, Ok(()));
            assert!(app.closes_out_of_sight(pane.id), "its strip is not shown");
        }
        let entry = crate::persistence::SavedConversation {
            card: pane.id,
            title: "Kept".into(),
            session: session.clone(),
            open: true,
            updated_at_unix_ms: 0,
        };
        let open_before = app.workspace.order().collect::<Vec<_>>();
        drop(app.resume_saved_conversation(entry));
        assert_eq!(
            app.resume_ui.error, None,
            "{case:?}: it is not open elsewhere"
        );
        let reopened = app
            .workspace
            .order()
            .find(|card| !open_before.contains(card))
            .unwrap_or_else(|| panic!("{case:?}: a new pane reopens it"));
        assert_ne!(reopened, pane.id);
        if case == WhileStoppingThenRefused {
            retire(&mut app, Err("the provider did not exit".into()));
            assert!(
                app.chat_start_pending(pane.id),
                "the closed pane goes back to what still runs"
            );
            assert!(!app.workspace.is_hidden(pane.id), "in sight");
            assert!(
                app.reopen_superseded(reopened),
                "the reopen gives way to it, rather than a second pane"
            );
            drop(app.update(Message::CommitClose(reopened)));
            assert!(
                !app.card_state(reopened)
                    .is_some_and(|state| state.existence.is_open())
            );
            assert!(!app.chat_start_pending(reopened));
            assert_eq!(
                app.draft_to_keep(pane.id, None).text,
                "still writing",
                "it keeps what it had not sent"
            );
            continue;
        }
        if !settled {
            assert!(app.holds_reopen(reopened), "it waits for the stop");
            retire(&mut app, Ok(()));
        }
        drop(app.answer_reopening(reopened, Ok(None)));
        assert_eq!(
            app.draft_to_keep(reopened, None).text,
            "still writing",
            "{case:?}: what the closed pane had not sent comes back"
        );
        if case == AfterStoppingThenRelaunch {
            let CardKind::Chat(reopened_session) = app.card_state(reopened).unwrap().kind.clone()
            else {
                panic!("a chat pane");
            };
            let mut next = crate::app::tests::context_inspector_test_app(false);
            next.durable = app.persistent_workspace();
            next.workspace = Cagents::rebuild_workspace(&next.durable);
            assert!(
                !next.awaits_stop(reopened, &reopened_session),
                "the reopened pane attaches at launch"
            );
            assert!(
                next.awaits_stop(pane.id, &session),
                "the closed one does not"
            );
            drop(next.resume_stopping_chats());
            assert!(!next.holds_reopen(reopened));
            assert!(
                next.background_close_for_test(pane.id).is_none()
                    && next.background_close_for_test(reopened).is_none(),
                "nothing is asked to stop again"
            );
            assert!(
                !next
                    .card_state(pane.id)
                    .is_some_and(|state| state.existence.is_open()),
                "the closed pane goes"
            );
            assert!(next.durable.stopping_chats.is_empty());
            continue;
        }
        if settled {
            app.workspace = strip;
            app.rehide_closing_panes();
            drop(app.remove_stopped_returns());
        }
        assert!(
            !app.card_state(pane.id)
                .is_some_and(|state| state.existence.is_open()),
            "{case:?}: the closed pane goes"
        );
        assert!(
            !app.recently_closed
                .iter()
                .any(|(closed, _)| closed.id == pane.id),
            "{case:?}: nor is it listed to reopen while its conversation is open"
        );
        assert!(
            app.card_state(reopened)
                .is_some_and(|state| state.existence.is_open())
        );
        assert_eq!(
            app.draft_to_keep(reopened, None).text,
            "still writing",
            "{case:?}: and stays with the reopened pane"
        );
        assert!(
            !app.durable.kept_chat_drafts.contains_key(&pane.id),
            "{case:?}: nor is it kept twice"
        );
        assert!(app.durable.stopping_chats.is_empty());
    }
}

/// A closed pane its view kept, restored at the next launch from that view
/// after its conversation stopped, holds what it had not sent in its
/// composer while it waits for admission. Reopening the conversation then
/// takes that into the new pane.
#[test]
fn a_kept_closed_pane_awaiting_admission_gives_up_what_it_had_not_sent() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, pane, stopping) =
        closed_chat_its_view_kept_from(Some(directory.path()), "still writing");
    let CardKind::Chat(session) = app.card_state(pane.id).unwrap().kind.clone() else {
        panic!("a chat pane");
    };
    app.workspace = Cagents::rebuild_workspace(&Default::default());
    drop(
        app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
            stopping,
            Ok(()),
        ))),
    );
    let saved = app.persistent_workspace();
    let mut next = crate::app::tests::context_inspector_test_app(false);
    next.chat_ui = super::super::chat_ui::State::with_drafts(saved.chat_drafts.clone());
    next.durable = saved;
    next.workspace = Cagents::rebuild_workspace(&next.durable);
    next.await_shared_chat_admission(pane.id, session.clone());
    drop(next.resume_stopping_chats());
    assert!(next.closes_out_of_sight(pane.id), "it waits out of sight");
    assert_eq!(next.draft_to_keep(pane.id, None).text, "still writing");
    let entry = crate::persistence::SavedConversation {
        card: pane.id,
        title: "Kept".into(),
        session: session.clone(),
        open: true,
        updated_at_unix_ms: 0,
    };
    let open_before = next.workspace.order().collect::<Vec<_>>();
    drop(next.resume_saved_conversation(entry));
    assert_eq!(next.resume_ui.error, None);
    let reopened = next
        .workspace
        .order()
        .find(|card| !open_before.contains(card))
        .expect("a new pane reopens it");
    drop(next.answer_reopening(reopened, Ok(None)));
    assert_eq!(
        next.draft_to_keep(reopened, None).text,
        "still writing",
        "it comes back"
    );
    assert_eq!(
        next.draft_to_keep(pane.id, None).text,
        "",
        "and is not left behind to be kept twice"
    );
}

/// A closed pane its view kept, whose conversation stopped while another
/// pane's removal held the strip, waits for that removal and then goes,
/// asking the daemon nothing (#3650): no second stop, which could reach a
/// conversation reopened meanwhile, and no new stop saved.
#[test]
fn a_kept_closed_pane_waits_for_the_strip_without_stopping_again() {
    let (mut app, pane, stopping) = closed_chat_its_view_kept();
    drop(app.insert_terminal_card(attention_terminal_source(), None));
    let other = app
        .workspace
        .order()
        .find(|card| *card != pane.id)
        .expect("another pane in the strip");
    app.workspace.request_close(other).unwrap();
    drop(
        app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
            stopping,
            Ok(()),
        ))),
    );
    assert!(
        app.card_state(pane.id)
            .is_some_and(|state| state.existence.is_open()),
        "it waits for the other removal"
    );
    assert!(app.closes_out_of_sight(pane.id), "out of sight meanwhile");
    assert!(
        app.background_close_for_test(pane.id).is_none(),
        "nothing is asked to stop again"
    );
    assert!(
        app.durable
            .stopping_chats
            .iter()
            .all(|stopping| stopping.stopped),
        "no new stop is saved"
    );
    // The other removal finishes, and the close queue settles as each
    // update ends.
    app.workspace.cancel_close(other).unwrap();
    drop(app.settle_close_queue());
    assert!(
        !app.card_state(pane.id)
            .is_some_and(|state| state.existence.is_open()),
        "it goes once the strip is free"
    );
    assert!(app.background_close_for_test(pane.id).is_none());
    assert!(app.durable.stopping_chats.is_empty());
}

/// At the next launch, a closed pane its view kept stays out of sight even
/// when the view admits it before the launch asks about its stop, and goes
/// once it has. A conversation reopened while that stop is asked again, then
/// refused, goes back to what still runs in the reopened pane, with what the
/// closed pane had not sent, when the view fences the closed one, which is
/// never admitted: that one goes, rather than stay beside it.
#[test]
fn a_kept_closed_pane_its_view_fences_leaves_a_refused_reopen_attached() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, pane, _stopping) =
        closed_chat_its_view_kept_from(Some(directory.path()), "still writing");
    let CardKind::Chat(session) = app.card_state(pane.id).unwrap().kind.clone() else {
        panic!("a chat pane");
    };
    let saved = app.persistent_workspace();
    let launch = || {
        let mut next = crate::app::tests::context_inspector_test_app(false);
        next.chat_ui = super::super::chat_ui::State::with_drafts(saved.chat_drafts.clone());
        next.durable = saved.clone();
        next.workspace = Cagents::rebuild_workspace(&next.durable);
        next
    };

    // Admitted before the launch asks again: still out of sight, and gone
    // once the stop settles.
    let mut next = launch();
    next.await_shared_chat_admission(pane.id, session.clone());
    let mut admitted = pane.clone();
    admitted.kind = CardKind::Chat(session.clone());
    drop(next.admit_shared_chat(&admitted));
    assert!(
        next.workspace.is_hidden(pane.id),
        "out of sight from the start"
    );
    assert!(!next.chat_start_pending(pane.id), "nothing attaches");
    drop(next.resume_stopping_chats());
    let target = next
        .background_close_for_test(pane.id)
        .expect("asked again");
    assert!(
        next.workspace.is_hidden(pane.id),
        "out of sight while asked"
    );
    drop(
        next.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
            target,
            Ok(()),
        ))),
    );
    assert!(
        !next
            .card_state(pane.id)
            .is_some_and(|state| state.existence.is_open()),
        "it goes"
    );

    // Fenced while it awaits admission, with a reopen during the stop: off
    // the strip shown, in it, and in it while another pane's removal holds
    // it.
    for (in_strip, busy) in [(false, false), (true, false), (true, true)] {
        let mut next = launch();
        next.await_shared_chat_admission(pane.id, session.clone());
        next.shared_views.quarantined.insert(pane.id);
        drop(next.resume_stopping_chats());
        let target = next
            .background_close_for_test(pane.id)
            .expect("asked again");
        let entry = crate::persistence::SavedConversation {
            card: pane.id,
            title: "Kept".into(),
            session: session.clone(),
            open: true,
            updated_at_unix_ms: 0,
        };
        let strip = next.workspace.order().collect::<Vec<_>>();
        drop(next.resume_saved_conversation(entry));
        let reopened = next
            .durable
            .cards
            .iter()
            .map(|card| card.id)
            .find(|card| next.holds_reopen(*card) && *card != pane.id)
            .expect("a new pane reopens it");
        assert!(!strip.contains(&reopened));
        if in_strip {
            next.workspace = Cagents::rebuild_workspace(&next.durable);
            next.rehide_closing_panes();
            assert!(
                next.workspace.order().any(|card| card == pane.id),
                "its strip is shown"
            );
        } else {
            next.workspace = Cagents::rebuild_workspace(&Default::default());
        }
        let other = busy.then(|| {
            drop(next.insert_terminal_card(attention_terminal_source(), None));
            let other = next
                .workspace
                .order()
                .find(|card| *card != pane.id && *card != reopened)
                .expect("another pane in the strip");
            next.workspace.request_close(other).unwrap();
            other
        });
        next.status = None;
        drop(
            next.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
                target,
                Err("the provider did not exit".into()),
            ))),
        );
        let case = format!("in_strip={in_strip} busy={busy}");
        assert!(
            !next.reopen_superseded(reopened),
            "{case}: it does not give way to a pane that never attaches"
        );
        assert!(!next.holds_reopen(reopened));
        assert!(
            next.chat_start_pending(reopened),
            "{case}: it goes back to what still runs"
        );
        assert_eq!(
            next.draft_to_keep(reopened, None).text,
            "still writing",
            "{case}: with what the closed pane had not sent"
        );
        if let Some(other) = other {
            assert!(
                next.card_state(pane.id)
                    .is_some_and(|state| state.existence.is_open()),
                "{case}: it waits for the other removal"
            );
            assert!(next.closes_out_of_sight(pane.id), "{case}: out of sight");
            assert_eq!(next.status, None, "{case}: with nothing to report");
            next.workspace.cancel_close(other).unwrap();
            drop(next.settle_close_queue());
        }
        assert!(
            !next
                .card_state(pane.id)
                .is_some_and(|state| state.existence.is_open()),
            "{case}: the fenced pane goes, rather than stay beside it"
        );
        assert!(
            next.shared_views.fenced_closes.contains(&pane.id),
            "{case}: its view hears that it closed, by identity"
        );
        assert!(!next.closes_out_of_sight(pane.id));
        assert!(
            !next.durable.kept_chat_drafts.contains_key(&pane.id),
            "{case}: its draft is not kept twice"
        );
    }
}

/// A closed pane its view kept and then fences, whose stop is refused, does
/// not come back on what still runs, which it could never attach to: it goes
/// by identity, keeping what it had not sent. A reopen made during the stop
/// goes back to what still runs and takes that; without one, the refusal's
/// notice stays to reopen the conversation.
#[test]
fn a_kept_closed_pane_its_view_fences_goes_when_its_stop_is_refused() {
    for reopen in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, pane, stopping) =
            closed_chat_its_view_kept_from(Some(directory.path()), "still writing");
        let CardKind::Chat(session) = app.card_state(pane.id).unwrap().kind.clone() else {
            panic!("a chat pane");
        };
        app.shared_views.quarantined.insert(pane.id);
        let reopened = reopen.then(|| {
            drop(
                app.resume_saved_conversation(crate::persistence::SavedConversation {
                    card: pane.id,
                    title: "Kept".into(),
                    session: session.clone(),
                    open: true,
                    updated_at_unix_ms: 0,
                }),
            );
            app.durable
                .cards
                .iter()
                .map(|card| card.id)
                .find(|card| app.holds_reopen(*card) && *card != pane.id)
                .expect("a new pane reopens it")
        });
        drop(
            app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
                stopping,
                Err("the provider did not exit".into()),
            ))),
        );
        assert!(
            !app.card_state(pane.id)
                .is_some_and(|state| state.existence.is_open()),
            "reopen={reopen}: the fenced pane goes at once, with nothing left to commit"
        );
        assert!(
            app.shared_views.fenced_closes.contains(&pane.id),
            "reopen={reopen}: its view hears that it closed, by identity"
        );
        assert!(!app.chat_start_pending(pane.id), "it never attaches");
        match reopened {
            Some(reopened) => {
                assert!(
                    app.chat_start_pending(reopened),
                    "the reopen goes back to what still runs"
                );
                assert_eq!(app.draft_to_keep(reopened, None).text, "still writing");
                assert!(!app.durable.kept_chat_drafts.contains_key(&pane.id));
            }
            None => {
                assert!(
                    app.has_close_failure_notices(),
                    "the refusal's notice stays to reopen it"
                );
                assert_eq!(
                    app.durable
                        .kept_chat_drafts
                        .get(&pane.id)
                        .map(|draft| draft.text.as_str()),
                    Some("still writing"),
                    "what it had not sent is kept for that"
                );
            }
        }
    }
}

/// What a closed chat had not sent comes back with the pane its view keeps
/// while the conversation stops, and is kept again when the pane goes, so
/// reopening the conversation still brings it back.
#[test]
fn a_kept_closed_pane_keeps_what_it_had_not_sent() {
    let (mut app, _source, _destination, pane) = retained_chat_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.begin_chat_close(pane.id));
    let stopping = app
        .background_close_for_test(pane.id)
        .expect("the conversation stops in the background");
    // What the close kept.
    let closed_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    Arc::make_mut(&mut app.durable).kept_chat_drafts.insert(
        pane.id,
        crate::model::KeptChatDraft {
            text: "still writing".into(),
            closed_at_unix_ms,
            ..Default::default()
        },
    );
    let (operation, scope, change) = app
        .shared_views
        .pending
        .iter()
        .find(|(_, _, change)| {
            matches!(change, Change::RemoveSource { before, .. } if before.id == pane.id)
        })
        .cloned()
        .expect("its removal is published");
    while rx.try_recv().is_ok() {}
    drop(app.refuse_conflicting_view_change(
        scope,
        operation,
        change,
        crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT.into(),
    ));
    assert!(app.workspace.is_hidden(pane.id), "back, out of sight");
    assert_eq!(
        app.draft_to_keep(pane.id, None).text,
        "still writing",
        "the pane holds it while it is back"
    );
    drop(
        app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
            stopping,
            Ok(()),
        ))),
    );
    assert!(
        !app.card_state(pane.id)
            .is_some_and(|state| state.existence.is_open()),
        "it goes"
    );
    assert_eq!(
        app.durable
            .kept_chat_drafts
            .get(&pane.id)
            .map(|draft| draft.text.as_str()),
        Some("still writing"),
        "kept again as it goes"
    );
}

/// Closing a retained native chat never opens a modal, and its cleanup is
/// never overtaken by a channel move (#2327, #2593). The first close takes
/// the pane away at once, so there is nothing left to move while the daemon
/// stops the conversation. Once the daemon has refused that stop, the
/// conversation reopens on the provider still running it, and closing it
/// again waits on its pane for the daemon, refusing every move meanwhile.
#[test]
fn channel_move_waits_for_retained_native_chat_cleanup_without_a_modal() {
    let (mut app, source, destination, pane) = retained_chat_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    assert!(app.can_move_pane_to_channel(pane.id, destination));
    let close = app.begin_chat_close(pane.id);
    assert!(!app.modal_is_open());
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    let stopping = app
        .background_close_for_test(pane.id)
        .expect("the conversation stops in the background");
    // The close itself is published; nothing after it may be.
    while rx.try_recv().is_ok() {}
    let before = app.workspace_model.clone();
    assert!(!app.can_move_pane_to_channel(pane.id, destination));
    drop(app.move_pane_to_channel(pane.id, destination));
    drop(app.update(Message::MovePaneToChannel {
        card: pane.id,
        channel: destination,
    }));
    drop(app.update(Message::PaneDropToChannel {
        generation: app.workspace_view_generation,
        card: pane.id,
        channel: destination,
    }));
    assert!(rx.try_recv().is_err(), "a closed pane cannot be moved");
    assert!(!app.pane_transfer_pending(pane.id));
    assert_eq!(app.workspace_model, before);
    assert_eq!(app.durable.active_channel, Some(source));
    drop(close);

    // The daemon refuses to stop it; it reopens on what still runs.
    let CardKind::Chat(session) = &pane.kind else {
        unreachable!()
    };
    drop(
        app.update(Message::ChatUi(super::super::chat_ui::Action::Retired(
            stopping,
            Err("the retained provider is still running".into()),
        ))),
    );
    drop(app.update(Message::ChatUi(
        super::super::chat_ui::Action::ReopenRefusedClose(session.id),
    )));
    let reopened = app.workspace.focused().expect("the conversation reopened");
    assert_ne!(reopened, pane.id);
    crate::app::chat_ui::tests::attach_start(
        &mut app,
        reopened,
        session.launch.session_id.as_deref(),
    );
    while rx.try_recv().is_ok() {}
    // Its next close is the reader dealing with what held it: it waits on
    // its pane, and nothing moves the pane meanwhile.
    let close = app.begin_chat_close(reopened);
    assert!(app.chat_start_pending(reopened));
    assert!(!app.modal_is_open());
    assert_lifecycle_refuses_moves(&mut app, &rx, source, destination, reopened);
    drop(close);
}

#[test]
fn pending_channel_move_refuses_native_close_and_switch_until_refused() {
    let (mut app, source, destination, pane) = retained_chat_move_fixture();
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let before = app.workspace_model.clone();
    drop(app.move_pane_to_channel(pane.id, destination));
    let ModelEdit::ChangeView {
        operation,
        change: Change::Transfer { .. },
        ..
    } = rx.try_recv().unwrap()
    else {
        panic!("transfer request")
    };
    assert!(app.pane_transfer_pending(pane.id));
    assert!(!app.chat_start_pending(pane.id));
    assert!(app.chat_switch_blocker(pane.id).is_some());
    drop(app.begin_chat_close(pane.id));
    drop(app.update(Message::CloseCard(pane.id)));
    drop(app.handle_chat_ui(super::super::chat_ui::Action::Switch(pane.id)));
    assert!(!app.chat_start_pending(pane.id));
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert_eq!(app.durable.active_channel, Some(source));
    assert_eq!(app.workspace_model, before);
    assert!(rx.try_recv().is_err());
    app.reject_pane_transfer(operation, "fixture rejected move".into());
    assert!(!app.pane_transfer_pending(pane.id));
    assert!(app.chat_switch_blocker(pane.id).is_none());
    let close = app.begin_chat_close(pane.id);
    assert!(
        !app.card_state(pane.id).unwrap().existence.is_open(),
        "once the move is refused the close goes ahead"
    );
    drop(close);
}

#[test]
fn pending_channel_move_refuses_bulk_channel_delete_before_close_capture() {
    let (mut app, source, destination, pane) =
        channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.move_pane_to_channel(pane.id, destination));
    let ModelEdit::ChangeView {
        operation,
        change: Change::Transfer { .. },
        ..
    } = rx.try_recv().unwrap()
    else {
        panic!("transfer request");
    };
    let close = app.delete_channel(source);
    assert!(!app.channel_ui.is_deleting(source));
    assert!(app.pane_transfer_pending(pane.id));
    assert_eq!(app.durable.active_channel, Some(source));
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert!(
        rx.try_recv().is_err(),
        "delete cannot capture or publish while the move owns its pane"
    );
    drop(close);
    app.reject_pane_transfer(operation, "fixture refusal".into());
    let close = app.delete_channel(source);
    assert!(
        app.channel_ui.is_deleting(source),
        "ordinary deletion is eligible after refusal"
    );
    assert!(!app.channel_ui.close_operation(source).is_nil());
    drop(close); // Keep the real production close task unpolled: no daemon effects.
}

#[test]
fn held_bulk_channel_delete_refuses_moves_from_its_source_only() {
    let (mut app, source, destination, pane) =
        channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let close = app.delete_channel(source);
    assert!(app.channel_ui.is_deleting(source));
    while rx.try_recv().is_ok() {}
    assert!(!app.pane_has_move_destination(pane.id));
    drop(app.move_pane_to_channel(pane.id, destination));
    drop(app.update(Message::MovePaneToChannel {
        card: pane.id,
        channel: destination,
    }));
    drop(app.update(Message::PaneDropToChannel {
        generation: app.workspace_view_generation,
        card: pane.id,
        channel: destination,
    }));
    assert!(!app.pane_transfer_pending(pane.id));
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::ChangeView {
                change: Change::Transfer { .. },
                ..
            }
        ))
    );
    let operation = app.channel_ui.close_operation(source);
    drop(
        app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
            source,
            operation,
            Err("fixture cleanup refused".into()),
        )),
    );
    assert!(
        app.can_move_pane_to_channel(pane.id, destination),
        "failed deletion releases only its local ownership"
    );
    drop(close);
}

#[test]
fn delayed_channel_close_never_retires_moved_replaced_or_new_members() {
    for changed in [
        "moved",
        "replacement",
        "model replacement",
        "missing shared pane",
        "new member",
        "replacement registry",
        "reopened member",
        "previously missing member",
    ] {
        let (mut app, source, destination, pane) =
            channel_move_fixture_with_kind(Some(moving_terminal_kind()));
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        let inactive = CardId::new();
        if matches!(changed, "reopened member" | "previously missing member") {
            Arc::make_mut(&mut app.durable)
                .channels
                .iter_mut()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .push(inactive);
            if changed == "reopened member" {
                let mut closed = app.card_state(pane.id).unwrap().clone();
                closed.id = inactive;
                closed.existence =
                    super::super::project_ui::removed_existence("fixture already closed");
                Arc::make_mut(&mut app.durable).cards.push(closed);
            }
        }
        let close = app.delete_channel(source);
        let operation = app.channel_ui.close_operation(source);
        while rx.try_recv().is_ok() {}
        match changed {
            "moved" => {
                views::transfer(
                    &mut app.workspace_model.views,
                    &Scope::Channel(source),
                    pane.id,
                    &Scope::Channel(destination),
                    None,
                )
                .unwrap();
                drop(app.reconcile_shared_views());
            }
            "replacement" | "model replacement" => {
                let CardKind::Terminal(mut successor) = pane.kind.clone() else {
                    unreachable!()
                };
                successor.id = TerminalId::new();
                successor.continuity = TerminalContinuity::Live {
                    host_session_id: crate::model::HostSessionId::new("successor-host"),
                    boot_id: crate::model::BootId::new("move-boot"),
                    host_incarnation: crate::model::HostIncarnation::new("successor-incarnation"),
                };
                if changed == "replacement" {
                    app.card_state_mut(pane.id).unwrap().kind = CardKind::Terminal(successor);
                } else {
                    app.workspace_model
                        .views
                        .iter_mut()
                        .flat_map(|view| &mut view.panes)
                        .find(|candidate| candidate.id == pane.id)
                        .unwrap()
                        .kind = CardKind::Terminal(successor);
                }
            }
            "replacement registry" => {
                let mut replacement = app.workspace_model.clone();
                replacement.identity = uuid::Uuid::new_v4();
                let identity = replacement.identity;
                drop(app.apply_workspace_model_update(
                    super::super::workspace_model_sync::ModelUpdate::Connected(replacement),
                ));
                assert_eq!(app.workspace_model.identity, identity);
                assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
            }
            "reopened member" => app.card_state_mut(inactive).unwrap().existence = Existence::Open,
            "previously missing member" => {
                let mut reopened = app.card_state(pane.id).unwrap().clone();
                reopened.id = inactive;
                Arc::make_mut(&mut app.durable).cards.push(reopened);
            }
            "missing shared pane" => app
                .workspace_model
                .views
                .iter_mut()
                .for_each(|view| view.panes.retain(|candidate| candidate.id != pane.id)),
            "new member" => Arc::make_mut(&mut app.durable)
                .channels
                .iter_mut()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .push(CardId::new()),
            _ => unreachable!(),
        }
        drop(
            app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                source,
                operation,
                Ok(()),
            )),
        );
        assert!(
            app.durable
                .channels
                .iter()
                .any(|channel| channel.id == source),
            "{changed}"
        );
        assert!(
            app.card_state(pane.id).unwrap().existence.is_open(),
            "{changed}"
        );
        assert!(!app.channel_ui.is_deleting(source));
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|edit| matches!(edit, ModelEdit::DeleteChannel { .. })),
            "{changed}"
        );
        drop(close);
    }
}

#[test]
fn retained_channel_close_accepts_its_projected_retirement_before_the_receipt() {
    for watcher_first in [false, true] {
        for sibling in [false, true] {
            let (mut app, source, _, pane) = retained_chat_move_fixture();
            if sibling {
                let mut other = pane.clone();
                other.id = CardId::new();
                other.kind = CardKind::Browser(BrowserSession {
                    id: BrowserId::new(),
                    current_url: "https://example.com".parse().unwrap(),
                    history: vec![],
                    history_index: 0,
                    scroll_x: 0.,
                    scroll_y: 0.,
                    fidelity: BrowserRestoreFidelity::UrlOnly,
                });
                app.workspace_model.views[0].panes.push(other);
                drop(app.reconcile_shared_views());
            }
            let (tx, rx) = test_model_channel();
            app.workspace_model_tx = tx;
            let close = app.delete_channel(source);
            let operation = app.channel_ui.close_operation(source);
            assert!(!operation.is_nil());
            while rx.try_recv().is_ok() {}

            if watcher_first {
                let view = app
                    .workspace_model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(source))
                    .unwrap();
                view.apply(&Change::Remove { id: pane.id }).unwrap();
                view.revision += 1;
                drop(app.reconcile_shared_views());
                assert!(!app.card_state(pane.id).unwrap().existence.is_open());
                assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
                let channel = app
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == source)
                    .unwrap();
                assert!(!channel.cards.contains(&pane.id));
                assert_eq!(channel.cards.len(), usize::from(sibling));
                assert!(app.channel_ui.is_deleting(source));
            }

            drop(
                app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                    source,
                    operation,
                    Ok(()),
                )),
            );
            if !watcher_first {
                let view = app
                    .workspace_model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(source))
                    .unwrap();
                view.apply(&Change::Remove { id: pane.id }).unwrap();
                view.revision += 1;
                drop(app.reconcile_shared_views());
            }
            assert!(
                !app.durable
                    .channels
                    .iter()
                    .any(|channel| channel.id == source),
                "watcher_first={watcher_first}, sibling={sibling}"
            );
            assert!(!app.channel_ui.is_deleting(source));
            assert!(!app.card_state(pane.id).unwrap().existence.is_open());
            assert_eq!(
                std::iter::from_fn(|| rx.try_recv().ok())
                    .filter(|edit| matches!(edit, ModelEdit::DeleteChannel { id } if *id == source))
                    .count(),
                1,
                "one successful close deletes its channel once"
            );
            // The registry deletes this view with the channel. A close
            // whose projection beat its receipt must still have a known
            // deleted origin after that last canonical record is gone.
            app.workspace_model
                .views
                .retain(|view| view.scope != Scope::Channel(source));
            assert_eq!(
                app.closed_card_home(pane.id),
                Some(super::super::channel_ui::ClosedCardHome::Strip),
                "watcher_first={watcher_first}, sibling={sibling}"
            );
            drop(close); // No provider request is polled by this ordering fixture.
        }
    }
}

#[test]
fn retained_channel_close_requires_source_retirement_without_replacement_or_new_members() {
    for changed in [
        "missing retirement",
        "wrong retirement scope",
        "replacement",
        "new member",
        "moved",
    ] {
        let (mut app, source, destination, pane) = retained_chat_move_fixture();
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        let close = app.delete_channel(source);
        let operation = app.channel_ui.close_operation(source);
        while rx.try_recv().is_ok() {}

        if changed == "moved" {
            views::transfer(
                &mut app.workspace_model.views,
                &Scope::Channel(source),
                pane.id,
                &Scope::Channel(destination),
                None,
            )
            .unwrap();
        } else {
            let view = app
                .workspace_model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(source))
                .unwrap();
            view.apply(&Change::Remove { id: pane.id }).unwrap();
            view.revision += 1;
            if matches!(changed, "missing retirement" | "wrong retirement scope") {
                view.retired.retain(|id| *id != pane.id);
            }
            if changed == "wrong retirement scope" {
                app.workspace_model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(destination))
                    .unwrap()
                    .retired
                    .push(pane.id);
            }
        }
        drop(app.reconcile_shared_views());
        if changed == "replacement" {
            let CardKind::Chat(session) = &mut app.card_state_mut(pane.id).unwrap().kind else {
                unreachable!()
            };
            session.id = TerminalId::new();
        }
        let new_member = (changed == "new member").then(CardId::new);
        if let Some(id) = new_member {
            Arc::make_mut(&mut app.durable)
                .channels
                .iter_mut()
                .find(|channel| channel.id == source)
                .unwrap()
                .cards
                .push(id);
        }
        let current = app.card_state(pane.id).unwrap().clone();
        drop(
            app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
                source,
                operation,
                Ok(()),
            )),
        );
        let channel = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap_or_else(|| panic!("{changed}: stale close deleted its channel"));
        if let Some(id) = new_member {
            assert!(channel.cards.contains(&id));
        }
        assert_eq!(app.card_state(pane.id).unwrap(), &current, "{changed}");
        assert!(!app.channel_ui.is_deleting(source));
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|edit| matches!(edit, ModelEdit::DeleteChannel { .. })),
            "{changed}"
        );
        drop(close);
    }
}

#[test]
fn channel_close_completion_is_attempt_bound_and_accepts_ordinary_terminal_exit() {
    let (mut app, source, _, pane) = channel_move_fixture_with_kind(Some(moving_terminal_kind()));
    let first = app.delete_channel(source);
    let old = app.channel_ui.close_operation(source);
    drop(
        app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
            source,
            old,
            Err("fixture retry".into()),
        )),
    );
    let second = app.handle_channel_ui(super::super::channel_ui::Action::RetryFailed(source));
    let current = app.channel_ui.close_operation(source);
    assert_ne!(old, current);
    drop(
        app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
            source,
            old,
            Ok(()),
        )),
    );
    assert!(
        app.channel_ui.is_deleting(source),
        "a late old completion cannot consume the new attempt"
    );
    assert_eq!(app.channel_ui.close_operation(source), current);
    app.retire_ended_terminal(pane.id, 0, None, true);
    assert!(
        terminal_ended(match &app.card_state(pane.id).unwrap().kind {
            CardKind::Terminal(session) => session,
            _ => unreachable!(),
        }),
        "the actual terminal event reducer produced its normal ended descriptor"
    );
    drop(
        app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
            source,
            current,
            Ok(()),
        )),
    );
    assert!(
        !app.durable
            .channels
            .iter()
            .any(|channel| channel.id == source)
    );
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    drop((first, second));
}

/// Unsent composer text belongs to the card. Another client replacing an
/// unstarted chat's identity, then moving the card to another channel, must
/// leave the text in the composer and in what autosave writes. That holds
/// when the replacement is already started, as when another client's Send
/// lands before this window sees its unstarted candidate.
#[test]
fn unsent_text_survives_replaced_identity_and_transfer() {
    use crate::app::workspace_model_sync::ModelUpdate;
    const TEXT: &str = "unsent fixture draft\n  keeps its spacing ";
    for (replace, transfer, started) in [
        (true, false, false),
        (true, true, false),
        (false, true, false),
        (true, false, true),
    ] {
        let case = format!("replace={replace} transfer={transfer} started={started}");
        let (mut app, a, b, _) = fixture();
        app.active_workspace = Some(PathBuf::from("/repo"));
        Arc::make_mut(&mut app.durable).active_channel = Some(a);
        drop(app.new_native_chat(crate::agent::Provider::Claude, Some("/repo".into())));
        let card = app.workspace.focused().unwrap();
        let target = match app.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime.target,
            _ => panic!("{case}: a new chat opens a draft runtime"),
        };
        drop(app.update(Message::ChatUi(chat_ui::Action::Composer(
            target,
            cosmic::widget::text_editor::Action::Edit(cosmic::widget::text_editor::Edit::Paste(
                Arc::new(TEXT.into()),
            )),
        ))));
        let mut pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
        app.shared_views.pending.clear();
        let mut model = app.workspace_model.clone();
        model.views = vec![View::empty(a), View::empty(b)];
        model.channels = [a, b]
            .into_iter()
            .map(|id| crate::workspace_model::ModelChannel {
                id,
                repository: PathBuf::from("/repo"),
                title: "view".into(),
                title_source: Default::default(),
                ordinal: 1,
                checkout: None,
            })
            .collect();
        model.views[0].panes.push(pane.clone());
        let apply =
            |app: &mut Cagents, model: &mut crate::workspace_model::WorkspaceModel, scope| {
                model.sequence += 1;
                for view in &mut model.views {
                    view.revision += 1;
                }
                drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope,
                    operation: None,
                    model: model.clone(),
                }));
            };
        apply(&mut app, &mut model, Scope::Channel(a));
        if replace {
            let CardKind::Chat(session) = &mut pane.kind else {
                unreachable!()
            };
            session.id = TerminalId::new();
            if started {
                session.launch.session_id = Some("fixture-started-conversation".into());
            }
            assert_eq!(session.is_unstarted(), !started, "{case}");
            model.views[0].panes = vec![pane.clone()];
            apply(&mut app, &mut model, Scope::Channel(a));
        }
        if started {
            // The started conversation attaches asynchronously and opens
            // its composer with the text it was handed.
            assert!(app.card_state(card).unwrap().existence.is_open(), "{case}");
            let ready = chat_ui::ready_message_for_pending_switch(
                &app,
                card,
                crate::chat::Snapshot::default(),
            );
            drop(app.update(ready));
            match app.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) => {
                    assert!(!runtime.session.is_unstarted(), "{case}: attached");
                    assert_eq!(runtime.unsent_text(), TEXT, "{case}: composer");
                }
                _ => panic!("{case}: the started conversation attaches"),
            }
            assert_eq!(
                app.persistent_workspace()
                    .chat_drafts
                    .get(&card)
                    .map(String::as_str),
                Some(TEXT),
                "{case}: autosave"
            );
            continue;
        }
        if transfer {
            model.views[0].panes.clear();
            model.views[1].panes = vec![pane.clone()];
            apply(&mut app, &mut model, Scope::Channel(b));
            // A repeated delivery must not open the draft a second time
            // without its text.
            apply(&mut app, &mut model, Scope::Channel(b));
        }
        let home = if transfer { b } else { a };
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == home)
                .is_some_and(|channel| channel.cards == [card]),
            "{case}: the card follows the view"
        );
        assert!(app.card_state(card).unwrap().existence.is_open(), "{case}");
        match app.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => {
                assert_eq!(runtime.unsent_text(), TEXT, "{case}: composer");
                assert_eq!(
                    Some(&runtime.session.id),
                    match &pane.kind {
                        CardKind::Chat(session) => Some(&session.id),
                        _ => None,
                    },
                    "{case}: the runtime follows the canonical identity"
                );
            }
            _ => panic!("{case}: the card keeps a chat runtime"),
        }
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&card)
                .map(String::as_str),
            Some(TEXT),
            "{case}: autosave"
        );
    }
}

#[test]
fn a_pane_reopening_in_a_channel_keeps_its_place_until_it_starts() {
    let (mut app, channel, _, first) = channel_move_fixture();
    // The channel on screen, whose strip is the order its panes are in.
    app.active_workspace = Some("/repo".into());
    let mut last = first.clone();
    last.id = CardId::new();
    if let CardKind::Browser(browser) = &mut last.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model.views[0].panes.push(last.clone());
    drop(app.reconcile_shared_views());
    let order = |app: &Cagents| app.workspace.order().collect::<Vec<_>>();
    assert_eq!(order(&app), [first.id, last.id]);

    let mut session = crate::chat::fresh_session(
        crate::agent::Provider::Claude,
        TerminalId::new(),
        "/repo".into(),
        &crate::account::Account::system(crate::agent::Provider::Claude),
    )
    .unwrap();
    // A conversation that ran, so starting it connects rather than drafts.
    session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
    let held = app.place_reopening_beside(first.id, session.clone());
    assert_eq!(order(&app), [first.id, held, last.id]);

    // The view does not list it until it starts, and it stays where it was
    // put rather than moving to the end of the strip.
    app.workspace_model.views[0].revision += 1;
    drop(app.reconcile_shared_views());
    assert_eq!(order(&app), [first.id, held, last.id]);
    let channel = app
        .durable
        .channels
        .iter()
        .find(|candidate| candidate.id == channel)
        .unwrap();
    assert_eq!(channel.cards, [first.id, held, last.id]);

    // Once it starts, it is published where it is, before the pane after it.
    drop(app.update(Message::ResumeConversation(
        crate::app::resume_ui::Action::Prepared(held, 0, Ok(Some(Box::new(session)))),
    )));
    assert!(!app.reopen_pending(held));
    drop(app.reconcile_shared_views());
    assert!(
        app.shared_views
            .pending
            .iter()
            .any(|(_, _, change)| matches!(
                change,
                Change::Insert { pane, before } if pane.id == held && *before == Some(last.id)
            )),
        "{:?}",
        app.shared_views.pending
    );
    assert_eq!(order(&app), [first.id, held, last.id]);
}
