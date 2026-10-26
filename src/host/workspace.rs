//! Layout writes own a serialized WAL connection. SQLite waits, fsync and model
//! hydration avoid HostState; only publishing the committed edge does. The rare
//! inferred-conversation-title operation retains its original resident guard.
use super::state::{ClientId, SharedHost};
use crate::{
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
    workspace_model::{ModelEdit, WorkspaceModel},
};
use std::time::Instant;

fn unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, message, false)
}

/// Subscribe before reading: an independently committed execution/layout edit
/// is present in this snapshot or already queued as an edge. Duplicate older
/// edges are harmless; a commit can never fall into a subscribe/read gap.
pub(super) fn snapshot(
    host: &SharedHost,
    watcher: Option<ClientId>,
) -> Result<WorkspaceModel, ProtocolError> {
    let writer = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let writer = state
            .workspace_registry
            .clone()
            .ok_or_else(|| unavailable("The workspace model writer is unavailable"))?;
        if let Some(watcher) = watcher {
            state.subscribe_model(watcher);
        }
        writer
    };
    let registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    registry.model().map_err(unavailable)
}

pub(super) fn edit(
    host: &SharedHost,
    origin: ClientId,
    edit: ModelEdit,
) -> Result<WorkspaceModel, ProtocolError> {
    let edit = edit
        .canonicalized()
        .map_err(|error| ProtocolError::new(ProtocolErrorCode::InvalidRequest, error, false))?;
    let writer = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .workspace_registry
        .clone()
        .ok_or_else(|| unavailable("The workspace model writer is unavailable"))?;
    // Keep this guard through publication so two concurrent clients cannot
    // publish sequence N+1 before N. No HostState operation acquires this mutex.
    let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    if matches!(&edit, ModelEdit::SuggestConversationTitle { .. }) {
        // This rare inference also authorizes against resident chat identity
        // and title metadata. Keep its original HostLock/commit atomicity;
        // the lock-free workspace-write claim excludes this operation.
        return host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .edit_model(origin, edit);
    }
    if let ModelEdit::ChangeView {
        scope,
        expected_revision,
        operation,
        change,
    } = &edit
    {
        let seen = registry
            .view_operation_seen(*operation)
            .map_err(unavailable)?;
        if !seen {
            if let Some(reason) = registry
                .view_change_refusal(scope, change)
                .map_err(unavailable)?
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    reason,
                    false,
                ));
            }
            if registry.view_revision(scope).map_err(unavailable)? != *expected_revision {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::ViewConflict,
                    "The workspace view changed; rebase this pane mutation on the current revision",
                    true,
                ));
            }
        }
    }
    let applied = registry.apply(&edit).map_err(|error| {
        if error == "workspace view revision conflict"
            || error == super::model::Registry::VIEW_SOURCE_CONFLICT
        {
            return ProtocolError::new(ProtocolErrorCode::ViewConflict, error, true);
        }
        // An execution writer can close a view after the preflight read.
        // apply has rolled back; inspect authoritative membership again
        // rather than making the client retry a permanently invalid edit.
        if let ModelEdit::ChangeView { scope, change, .. } = &edit {
            match registry.view_change_refusal(scope, change) {
                Ok(Some(reason)) => {
                    return ProtocolError::new(ProtocolErrorCode::InvalidRequest, reason, false);
                }
                Err(storage) => return unavailable(storage),
                Ok(None) => {}
            }
        }
        unavailable(error)
    })?;
    if applied.changed {
        let waiting = Instant::now();
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let acquired = Instant::now();
        state.announce_model_edit(origin, applied.sequence, &edit);
        drop(state);
        tracing::debug!(
            operation = "workspace-model-edit",
            sequence = applied.sequence,
            host_lock_wait_us = acquired.duration_since(waiting).as_micros() as u64,
            host_lock_hold_us = acquired.elapsed().as_micros() as u64,
            "Published a workspace model edit"
        );
        crate::daemon::wake();
    }
    Ok(applied.model)
}

/// Obtain the existing writer admission without keeping HostState borrowed.
pub(super) fn writer(
    host: &SharedHost,
) -> Result<std::sync::Arc<std::sync::Mutex<super::model::Registry>>, ProtocolError> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .workspace_registry
        .clone()
        .ok_or_else(|| unavailable("The workspace model writer is unavailable"))
}

/// A model reservation read under the writer admission. Keep that admission
/// through worker publication and completion; this is not process authority.
pub(super) struct PendingTerminal {
    pub(super) session: crate::model::TerminalSession,
    pub(super) pane: crate::workspace_model::views::Pane,
    scope: crate::workspace_model::views::Scope,
    revision: u64,
}

pub(super) fn pending_terminal(
    registry: &super::model::Registry,
    pane: crate::model::CardId,
    terminal_id: crate::model::TerminalId,
) -> Result<PendingTerminal, ProtocolError> {
    for view in registry.model().map_err(unavailable)?.views {
        for candidate in view.panes {
            if candidate.id == pane {
                if let crate::model::CardKind::Terminal(session) = &candidate.kind {
                    if session.id == terminal_id {
                        return Ok(PendingTerminal {
                            session: session.clone(),
                            pane: candidate,
                            scope: view.scope,
                            revision: view.revision,
                        });
                    }
                }
            }
        }
    }
    Err(ProtocolError::new(
        ProtocolErrorCode::TerminalNotFound,
        "The automatic terminal reservation changed",
        false,
    ))
}

/// Ignore only provider-observed hint metadata. Every launch and saved source
/// field still belongs to the exact accepted reservation.
pub(super) fn matches_pending(
    accepted: &crate::model::TerminalSession,
    current: &crate::model::TerminalSession,
) -> bool {
    let mut current = current.clone();
    current.resume_hint = accepted.resume_hint.clone();
    accepted == &current
}

/// A freshly published holder replaces only its accepted relaunch projection
/// with this daemon's authenticated source. The caller separately binds both
/// sessions to the same original worker and its captured pending intent.
pub(super) fn matches_pending_issued(
    accepted: &crate::model::TerminalSession,
    issued: &crate::model::TerminalSession,
    proof: &crate::model::HostSessionProof,
) -> bool {
    let crate::model::TerminalContinuity::RelaunchAvailable { recipe, .. } = &accepted.continuity
    else {
        return false;
    };
    if recipe != &accepted.launch
        || !proof.is_complete()
        || !matches!(
            issued.continuity,
            crate::model::TerminalContinuity::DetachedSameBoot { .. }
        )
        || issued.continuity.live_proof().as_ref() != Some(proof)
    {
        return false;
    }
    let mut projected = issued.clone();
    projected.continuity = accepted.continuity.clone();
    matches_pending(accepted, &projected)
}

/// Caller retains the same writer guard used for the authoritative reservation
/// read and brief worker publication. No SQLite operation borrows HostState.
pub(super) fn complete_terminal(
    host: &SharedHost,
    registry: &mut super::model::Registry,
    reservation: PendingTerminal,
    original: &crate::terminal::TerminalHandle,
) -> Result<(), String> {
    let session = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        super::state::pending_worker_source(
            &state,
            reservation.pane.id,
            &reservation.session,
            original,
        )
        .ok_or("The automatic terminal source changed before completion")?
    };
    if !reservation.pane.pending_terminal_start {
        return Ok(());
    }
    let mut pane = reservation.pane.clone();
    pane.pending_terminal_start = false;
    pane.kind = crate::model::CardKind::Terminal(session);
    let edit = ModelEdit::ChangeView {
        scope: reservation.scope,
        expected_revision: reservation.revision,
        operation: uuid::Uuid::new_v4(),
        change: crate::workspace_model::views::Change::Update {
            before: reservation.pane,
            pane,
        },
    };
    let applied = registry.apply(&edit)?;
    if applied.changed {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .announce_model_edit(0, applied.sequence, &edit);
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static BEFORE_PENDING_PUBLICATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(super) fn pending_publication_test_hook() {
    let hook = BEFORE_PENDING_PUBLICATION.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::{
            model::Registry,
            state::{HostLock, HostState},
        },
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    };
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };

    #[test]
    fn public_edits_keep_transfer_refusals_and_source_conflicts_after_writer_move() {
        use crate::{
            model::{BrowserId, BrowserSession, CardId, CardKind, ChannelId},
            terminal_ipc::ClientRequest,
            workspace_model::views::{Change, Pane, Scope},
        };
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: directory.path().into(),
                name: "Fixture".into(),
                kind: Default::default(),
            })
            .unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: directory.path().into(),
                title: "Fixture".into(),
                title_source: Default::default(),
            })
            .unwrap();
        let scope = Scope::Channel(channel);
        let original = Pane {
            id: CardId::new(),
            title: "Browser".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: Some(directory.path().into()),
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com/a".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![original.clone()],
                closed: vec![],
            })
            .unwrap();
        let mut successor = original.clone();
        let CardKind::Browser(browser) = &mut successor.kind else {
            unreachable!()
        };
        browser.current_url = "https://example.com/b".parse().unwrap();
        registry
            .apply(&ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: registry.view_revision(&scope).unwrap(),
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before: original.clone(),
                    pane: successor.clone(),
                },
            })
            .unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("workspace-errors"),
            boot_id: BootId::new("fixture"),
            host_incarnation: HostIncarnation::new("fixture"),
        });
        state.install_model(registry);
        let host = Arc::new(HostLock::new(state));
        let before = snapshot(&host, None).unwrap();
        let revision = before
            .views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap()
            .revision;
        let mut stale = original.clone();
        stale.scope = Some(directory.path().join("changed"));
        let stale_operation = uuid::Uuid::new_v4();
        let missing_operation = uuid::Uuid::new_v4();
        for (operation, expected_revision, change, expected) in [
            (
                stale_operation,
                revision,
                Change::Update {
                    before: original,
                    pane: stale,
                },
                ProtocolErrorCode::ViewConflict,
            ),
            (
                missing_operation,
                0,
                Change::Transfer {
                    id: successor.id,
                    destination: Scope::Channel(ChannelId::new()),
                    before: None,
                },
                ProtocolErrorCode::InvalidRequest,
            ),
        ] {
            let error = super::super::client::process_request(
                &host,
                2,
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::ChangeView {
                        scope: scope.clone(),
                        expected_revision,
                        operation,
                        change,
                    },
                },
            )
            .unwrap_err();
            assert_eq!(error.code, expected);
            assert_eq!(snapshot(&host, None).unwrap(), before);
            let writer = writer(&host).unwrap();
            assert!(
                !writer
                    .lock()
                    .unwrap()
                    .view_operation_seen(operation)
                    .unwrap()
            );
        }
    }

    #[test]
    fn sqlite_writer_contention_does_not_hold_the_global_host_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let registry = Registry::at(&path).unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("workspace-lock"),
            boot_id: BootId::new("fixture"),
            host_incarnation: HostIncarnation::new("fixture"),
        });
        state.install_model(registry);
        let host = Arc::new(HostLock::new(state));
        let blocker = rusqlite::Connection::open(path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let writer_registry = host.lock().unwrap().workspace_registry.clone().unwrap();
        let edit_host = host.clone();
        let repository = directory.path().to_owned();
        let (started, ready) = mpsc::channel();
        let writer = thread::spawn(move || {
            started.send(()).unwrap();
            super::super::client::process_request(
                &edit_host,
                1,
                crate::terminal_ipc::ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::OpenProject {
                        repository,
                        name: "fixture".into(),
                        kind: Default::default(),
                    },
                },
            )
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        // Observe the writer owning its independent connection while SQLite
        // is blocked, rather than guessing that its thread ran after a sleep.
        let deadline = Instant::now() + Duration::from_secs(1);
        while writer_registry.try_lock().is_ok() {
            assert!(
                Instant::now() < deadline,
                "workspace writer did not enter its SQLite transaction"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let (acquired, receipt) = mpsc::channel();
        let reader_host = host.clone();
        let reader = thread::spawn(move || {
            let _state = reader_host.lock().unwrap();
            let _ = acquired.send(());
        });
        let responsive = receipt.recv_timeout(Duration::from_millis(500)).is_ok();
        blocker.execute_batch("ROLLBACK").unwrap();
        reader.join().unwrap();
        assert!(writer.join().unwrap().is_ok());
        assert!(
            responsive,
            "SQLite writer contention blocked terminal delivery's host lock"
        );
        assert_eq!(host.lock().unwrap().read_model().unwrap().projects.len(), 1);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod pending_tests;

#[cfg(all(test, target_os = "linux"))]
mod progress_tests;
