use super::*;
use crate::{
    terminal::RenderStreamFixture,
    workspace_model::views::{TerminalRemovalBasis, View},
};

fn prepared() -> (
    tempfile::TempDir,
    SharedHost,
    RenderStreamFixture,
    Pane,
    ModelEdit,
) {
    let (directory, host, _) = fixture();
    edit(&host, 1, open_project(directory.path())).unwrap();
    let launch = LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    };
    // No PTY, process, provider or worker thread is created by this fixture.
    let worker = RenderStreamFixture::new(|_| {});
    let descriptor = host
        .lock()
        .unwrap()
        .finish_spawn(1, session, worker.handle.clone(), false)
        .unwrap();
    let pane = Pane {
        id: CardId::new(),
        title: "Shared fixture".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Terminal(descriptor.session),
    };
    let scope = Scope::Worktree {
        repository: directory.path().to_owned(),
        path: directory.path().to_owned(),
    };
    let model = edit(
        &host,
        1,
        ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        },
    )
    .unwrap();
    let removal = ModelEdit::ChangeView {
        scope,
        expected_revision: model.views[0].revision,
        operation: uuid::Uuid::new_v4(),
        change: Change::RemoveSource {
            registry: model.identity,
            repository: directory.path().to_owned(),
            before: pane.clone(),
            terminal: Some(TerminalRemovalBasis::Worker {
                owner: host.lock().unwrap().proof().clone(),
                instance: worker.handle.execution_instance(),
            }),
        },
    };
    (directory, host, worker, pane, removal)
}

fn terminal(pane: &Pane) -> TerminalSession {
    let CardKind::Terminal(session) = &pane.kind else {
        panic!("terminal")
    };
    session.clone()
}

fn assert_refused(host: &SharedHost, removal: &ModelEdit) {
    let before = snapshot(host, None).unwrap();
    assert!(edit(host, 1, removal.clone()).is_err());
    assert_eq!(snapshot(host, None).unwrap(), before);
    let ModelEdit::ChangeView { operation, .. } = removal else {
        unreachable!()
    };
    assert!(
        !writer(host)
            .unwrap()
            .lock()
            .unwrap()
            .view_operation_seen(*operation)
            .unwrap()
    );
}

#[test]
fn shared_close_fences_same_id_worker_replacement_even_after_retirement() {
    let (_directory, host, _original, pane, removal) = prepared();
    let replacement = RenderStreamFixture::new(|_| {});
    let source = terminal(&pane);
    let before = snapshot(&host, None).unwrap();
    host.lock()
        .unwrap()
        .finish_spawn(2, source.clone(), replacement.handle.clone(), false)
        .unwrap();
    assert_eq!(
        snapshot(&host, None).unwrap(),
        before,
        "the replacement preceded its pane publication"
    );
    assert_refused(&host, &removal);
    host.lock().unwrap().terminals.remove(&source.id);
    assert_refused(&host, &removal);
}

#[test]
fn shared_close_fences_pending_terminal_and_native_starts() {
    for native in [false, true] {
        let (_directory, host, _worker, pane, removal) = prepared();
        let source = terminal(&pane);
        if native {
            let proof = host.lock().unwrap().proof().clone();
            host.lock().unwrap().chat_spawning.insert(
                source.id,
                crate::chat::Session {
                    id: source.id,
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Claude,
                        recipe: source.launch.clone(),
                        session_id: None,
                    },
                    proof: Some(proof),
                    terminal: None,
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                },
            );
        } else {
            let mut state = host.lock().unwrap();
            state.terminals.remove(&source.id);
            state.reserve_spawn(source.id).unwrap();
        }
        assert_refused(&host, &removal);
    }
}

#[test]
fn proven_absence_is_fenced_against_publish_retire_and_daemon_change() {
    let (_directory, host, _worker, pane, mut removal) = prepared();
    let source = terminal(&pane);
    host.lock().unwrap().terminals.remove(&source.id);
    let absence = host
        .lock()
        .unwrap()
        .terminal_removal_basis(&source)
        .unwrap();
    let ModelEdit::ChangeView {
        change: Change::RemoveSource { terminal, .. },
        ..
    } = &mut removal
    else {
        unreachable!()
    };
    *terminal = Some(absence);
    let replacement = RenderStreamFixture::new(|_| {});
    host.lock()
        .unwrap()
        .finish_spawn(2, source.clone(), replacement.handle.clone(), false)
        .unwrap();
    host.lock().unwrap().terminals.remove(&source.id);
    assert_refused(&host, &removal);
    let mut fresh = host
        .lock()
        .unwrap()
        .terminal_removal_basis(&source)
        .unwrap();
    if let TerminalRemovalBasis::Absent { owner, .. } = &mut fresh {
        owner.host_incarnation = "another-daemon".into();
    }
    let ModelEdit::ChangeView {
        change: Change::RemoveSource { terminal, .. },
        ..
    } = &mut removal
    else {
        unreachable!()
    };
    *terminal = Some(fresh);
    assert_refused(&host, &removal);
    let fresh = host
        .lock()
        .unwrap()
        .terminal_removal_basis(&source)
        .unwrap();
    let ModelEdit::ChangeView {
        change: Change::RemoveSource { terminal, .. },
        ..
    } = &mut removal
    else {
        unreachable!()
    };
    *terminal = Some(fresh);
    let removed = edit(&host, 1, removal.clone()).unwrap();
    assert!(removed.views[0].panes.is_empty());
    // A lost acknowledgement replays the receipt without acquiring authority
    // over a later worker or changing the already committed model sequence.
    host.lock()
        .unwrap()
        .finish_spawn(2, source, replacement.handle.clone(), false)
        .unwrap();
    assert_eq!(edit(&host, 1, removal).unwrap(), removed);
}

#[test]
fn attachment_worker_marker_identifies_the_attached_handle() {
    let (_directory, host, worker, pane, _) = prepared();
    let (_, extras, _) = process_request(
        &host,
        2,
        ClientRequest::Attach {
            terminal_id: terminal(&pane).id,
            after_generation: 0,
            after_event_sequence: 0,
        },
    )
    .unwrap();
    assert!(
        matches!(extras.first(), Some(ServerMessage::TerminalWorker { terminal_id, instance })
        if *terminal_id == terminal(&pane).id && *instance == worker.handle.execution_instance())
    );
    assert!(matches!(
        extras.last(),
        Some(ServerMessage::Synchronized { .. })
    ));
}

#[test]
fn completed_pane_without_a_surface_can_read_the_exact_worker_receipt() {
    let (_directory, host, worker, pane, _) = prepared();
    let mut ended = terminal(&pane);
    ended.continuity = TerminalContinuity::Exited {
        last_exit: crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    let basis = host.lock().unwrap().terminal_removal_basis(&ended).unwrap();
    assert!(
        matches!(basis, TerminalRemovalBasis::Worker { instance, .. } if instance == worker.handle.execution_instance())
    );
    ended.launch.arguments.push("foreign-source".into());
    assert!(host.lock().unwrap().terminal_removal_basis(&ended).is_err());
}
