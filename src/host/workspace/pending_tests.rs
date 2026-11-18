use super::*;
use crate::{
    host::{
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{
        BootId, CardId, CardKind, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
        TerminalContinuity, TerminalId, TerminalSession,
    },
    terminal_ipc::{ClientRequest, TerminalSpawnOptions},
    workspace_model::views::{Pane, Scope, View},
};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("pending-completion-test"),
        boot_id: BootId::new("pending-completion-test"),
        host_incarnation: HostIncarnation::new("pending-completion-test"),
    }
}

fn session(directory: &std::path::Path) -> TerminalSession {
    let launch = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf 'started\\n' >> launches; exec cat".into(),
        ],
        working_directory: directory.into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
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
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    path: PathBuf,
    scope: Scope,
    pane: Pane,
    session: TerminalSession,
    host: SharedHost,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: directory.path().into(),
                name: "fixture".into(),
                kind: Default::default(),
            })
            .unwrap();
        let scope = Scope::Worktree {
            repository: directory.path().into(),
            path: directory.path().into(),
        };
        let session = session(directory.path());
        let pane = Pane {
            id: CardId::new(),
            title: "terminal".into(),
            user_title: None,
            pending_terminal_start: true,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Terminal(session.clone()),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        let mut state = HostState::new(proof());
        state.install_model(registry);
        state.register_client(1);
        let host = Arc::new(HostLock::new(state));
        Self {
            directory,
            path,
            scope,
            pane,
            session,
            host,
        }
    }

    fn complete(&self) -> Result<(), ProtocolError> {
        crate::host::client::process_request(
            &self.host,
            1,
            ClientRequest::CompletePendingTerminal {
                pane: self.pane.id,
                terminal_id: self.session.id,
                options: TerminalSpawnOptions::default(),
            },
        )
        .map(|_| ())
    }

    fn view(&self) -> View {
        snapshot(&self.host, None)
            .unwrap()
            .views
            .into_iter()
            .find(|view| view.scope == self.scope)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let (handles, _) = self
            .host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .graceful_shutdown();
        for handle in handles {
            let _ = handle.shutdown();
        }
    }
}

#[test]
fn pending_completion_rebases_after_neighboring_pane_retirement() {
    let fixture = Fixture::new();
    let writer = writer(&fixture.host).unwrap();
    let mut registry = writer.lock().unwrap();
    let mut neighbor = fixture.pane.clone();
    neighbor.id = CardId::new();
    neighbor.kind = CardKind::Terminal(session(fixture.directory.path()));
    let revision = registry.view_revision(&fixture.scope).unwrap();
    registry
        .apply(&ModelEdit::ChangeView {
            scope: fixture.scope.clone(),
            expected_revision: revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Insert {
                pane: neighbor.clone(),
                before: None,
            },
        })
        .unwrap();
    let reservation = pending_terminal(&registry, fixture.pane.id, fixture.session.id).unwrap();
    // Execution retirement uses another connection, outside writer admission.
    let mut execution = Registry::at(&fixture.path).unwrap();
    execution
        .apply(&ModelEdit::ChangeView {
            scope: fixture.scope.clone(),
            expected_revision: reservation.revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Remove { id: neighbor.id },
        })
        .unwrap();
    complete_terminal(
        &fixture.host,
        &mut registry,
        reservation,
        fixture.session.clone(),
    )
    .unwrap();
    let view = registry.view_snapshot(&fixture.scope).unwrap().0.unwrap();
    assert_eq!(view.panes.len(), 1);
    assert!(!view.panes[0].pending_terminal_start);
}

#[test]
fn failed_pending_save_still_publishes_the_worker_and_retry_reuses_it() {
    let fixture = Fixture::new();
    // A real schema object refuses only the completion save; reading the
    // reservation and starting the worker are unaffected.
    rusqlite::Connection::open(&fixture.path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_pending BEFORE UPDATE ON view_scopes BEGIN SELECT RAISE(ABORT, 'fixture'); END;",
        )
        .unwrap();
    let error = fixture.complete().unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::Internal);
    let original = fixture.host.lock().unwrap().terminals[&fixture.session.id]
        .handle
        .clone();
    assert!(!original.is_completed());
    assert!(fixture.view().panes[0].pending_terminal_start);
    rusqlite::Connection::open(&fixture.path)
        .unwrap()
        .execute_batch("DROP TRIGGER reject_pending")
        .unwrap();
    fixture.complete().unwrap();
    let state = fixture.host.lock().unwrap();
    assert_eq!(state.terminals.len(), 1);
    assert!(
        state.terminals[&fixture.session.id]
            .handle
            .same_worker(&original)
    );
    let issued = state.descriptor(0, fixture.session.id).unwrap().session;
    drop(state);
    let saved = fixture.view();
    assert!(!saved.panes[0].pending_terminal_start);
    assert_eq!(saved.panes[0].kind, CardKind::Terminal(issued));
    let deadline = Instant::now() + Duration::from_secs(3);
    let launches = loop {
        if let Ok(launches) = fs::read_to_string(fixture.directory.path().join("launches"))
            && !launches.is_empty()
        {
            break launches;
        }
        assert!(Instant::now() < deadline, "original process did not start");
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(launches, "started\n");
}

#[test]
fn persistence_only_retry_saves_the_original_worker_without_starting_another() {
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_pending BEFORE UPDATE ON view_scopes BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    assert_eq!(
        fixture.complete().unwrap_err().code,
        ProtocolErrorCode::Internal
    );
    let original = fixture.host.lock().unwrap().terminals[&fixture.session.id]
        .handle
        .clone();
    db.execute_batch("DROP TRIGGER reject_pending").unwrap();
    for _ in 0..2 {
        crate::host::client::process_request(
            &fixture.host,
            1,
            ClientRequest::RetryPendingTerminalSave {
                pane: fixture.pane.id,
                terminal_id: fixture.session.id,
            },
        )
        .unwrap();
    }
    assert!(!fixture.view().panes[0].pending_terminal_start);
    let state = fixture.host.lock().unwrap();
    assert_eq!(state.terminals.len(), 1);
    assert!(
        state.terminals[&fixture.session.id]
            .handle
            .same_worker(&original)
    );
}

#[test]
fn pending_completion_refuses_changes_to_the_reserved_pane() {
    for mutation in ["title", "width", "source", "pending", "remove", "transfer"] {
        let fixture = Fixture::new();
        let writer = writer(&fixture.host).unwrap();
        let mut registry = writer.lock().unwrap();
        let reservation = pending_terminal(&registry, fixture.pane.id, fixture.session.id).unwrap();
        let mut execution = Registry::at(&fixture.path).unwrap();
        let change = match mutation {
            "remove" => Change::Remove {
                id: fixture.pane.id,
            },
            "transfer" => {
                let channel = crate::model::ChannelId::new();
                execution
                    .apply(&ModelEdit::CreateChannel {
                        id: channel,
                        repository: fixture.directory.path().into(),
                        title: "destination".into(),
                        title_source: Default::default(),
                    })
                    .unwrap();
                Change::Transfer {
                    id: fixture.pane.id,
                    destination: Scope::Channel(channel),
                    before: None,
                }
            }
            _ => {
                let mut pane = fixture.pane.clone();
                match mutation {
                    "title" => pane.user_title = Some("renamed".into()),
                    "width" => pane.width_fraction = 0.75,
                    "source" => pane.kind = CardKind::Terminal(session(fixture.directory.path())),
                    "pending" => pane.pending_terminal_start = false,
                    _ => unreachable!(),
                }
                Change::Update {
                    before: fixture.pane.clone(),
                    pane,
                }
            }
        };
        execution
            .apply(&ModelEdit::ChangeView {
                scope: fixture.scope.clone(),
                expected_revision: reservation.revision,
                operation: uuid::Uuid::new_v4(),
                change,
            })
            .unwrap();
        let before = registry.model().unwrap();
        let error = complete_terminal(
            &fixture.host,
            &mut registry,
            reservation,
            fixture.session.clone(),
        )
        .unwrap_err();
        assert!(
            matches!(
                error.code,
                ProtocolErrorCode::ViewConflict | ProtocolErrorCode::InvalidRequest
            ),
            "{mutation}: {error:?}"
        );
        assert_eq!(registry.model().unwrap(), before, "{mutation}");
    }
}

#[test]
fn failed_pending_save_cannot_retarget_the_retained_worker() {
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_pending BEFORE UPDATE ON view_scopes BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    assert_eq!(
        fixture.complete().unwrap_err().code,
        ProtocolErrorCode::Internal
    );
    let original = fixture.host.lock().unwrap().terminals[&fixture.session.id]
        .handle
        .clone();
    db.execute_batch("DROP TRIGGER reject_pending").unwrap();
    let mut pane = fixture.pane.clone();
    // The launch recipe and terminal ID still match. Accepting only those
    // would silently reinterpret the original worker's publication intent.
    pane.user_title = Some("changed while completion was refused".into());
    edit_delta(
        &fixture.host,
        1,
        ModelEdit::ChangeView {
            scope: fixture.scope.clone(),
            expected_revision: fixture.view().revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: fixture.pane.clone(),
                pane: pane.clone(),
            },
        },
    )
    .unwrap();
    let error = fixture.complete().unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::ViewConflict);
    assert_eq!(fixture.view().panes, vec![pane]);
    let state = fixture.host.lock().unwrap();
    assert_eq!(state.terminals.len(), 1);
    assert!(
        state.terminals[&fixture.session.id]
            .handle
            .same_worker(&original)
    );
}

#[test]
fn pending_start_refuses_source_change_between_reservation_reads() {
    let fixture = Fixture::new();
    let host = fixture.host.clone();
    let scope = fixture.scope.clone();
    let revision = fixture.view().revision;
    let mut newer = fixture.pane.clone();
    let CardKind::Terminal(source) = &mut newer.kind else {
        unreachable!()
    };
    source.resume_hint = Some("newer-pending-intent".into());
    let expected = newer.clone();
    let before = fixture.pane.clone();
    BEFORE_PENDING_START.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            edit_delta(
                &host,
                1,
                ModelEdit::ChangeView {
                    scope,
                    expected_revision: revision,
                    operation: uuid::Uuid::new_v4(),
                    change: Change::Update {
                        before,
                        pane: newer,
                    },
                },
            )
            .unwrap();
        }))
    });

    let error = fixture.complete().unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::ViewConflict);
    assert!(fixture.host.lock().unwrap().terminals.is_empty());
    assert_eq!(fixture.view().panes, vec![expected]);
    assert!(!fixture.directory.path().join("launches").exists());
}

#[test]
fn retry_reports_pane_withdrawal_between_reservation_reads() {
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_pending BEFORE UPDATE ON view_scopes BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    assert_eq!(
        fixture.complete().unwrap_err().code,
        ProtocolErrorCode::Internal
    );
    let original = fixture.host.lock().unwrap().terminals[&fixture.session.id]
        .handle
        .clone();
    db.execute_batch("DROP TRIGGER reject_pending").unwrap();
    let host = fixture.host.clone();
    let scope = fixture.scope.clone();
    let revision = fixture.view().revision;
    let pane = fixture.pane.id;
    BEFORE_PENDING_START.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            edit_delta(
                &host,
                1,
                ModelEdit::ChangeView {
                    scope,
                    expected_revision: revision,
                    operation: uuid::Uuid::new_v4(),
                    change: Change::Remove { id: pane },
                },
            )
            .unwrap();
        }))
    });
    let error = fixture.complete().unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::ViewConflict);
    assert!(fixture.view().panes.is_empty());
    assert!(
        fixture.host.lock().unwrap().terminals[&fixture.session.id]
            .handle
            .same_worker(&original)
    );
}

#[test]
fn retry_save_after_inventory_and_worker_retirement_cannot_launch_again() {
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_pending BEFORE UPDATE ON view_scopes BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
    assert_eq!(
        fixture.complete().unwrap_err().code,
        ProtocolErrorCode::Internal
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let launches = fixture.directory.path().join("launches");
    while fs::read_to_string(&launches).unwrap_or_default() != "started\n" {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(fixture.host.lock().unwrap().list(1).len(), 1);
    db.execute_batch("DROP TRIGGER reject_pending").unwrap();
    // Another client closes the exact worker after the UI's inventory read,
    // while the pending pane still records its original automatic-start intent.
    crate::host::client::process_request(
        &fixture.host,
        1,
        ClientRequest::ShutdownTerminal {
            terminal_id: fixture.session.id,
        },
    )
    .unwrap();
    assert!(fixture.host.lock().unwrap().terminals.is_empty());
    assert!(fixture.view().panes[0].pending_terminal_start);
    let error = crate::host::client::process_request(
        &fixture.host,
        1,
        ClientRequest::RetryPendingTerminalSave {
            pane: fixture.pane.id,
            terminal_id: fixture.session.id,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalNotFound);
    assert!(fixture.host.lock().unwrap().terminals.is_empty());
    assert_eq!(fs::read_to_string(launches).unwrap(), "started\n");
}
