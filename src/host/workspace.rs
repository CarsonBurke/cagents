//! Layout writes own a serialized WAL connection. SQLite waits, fsync and model
//! hydration never borrow HostState; only publishing the committed edge does.
use super::state::{ClientId, SharedHost};
use crate::{terminal_ipc::{ProtocolError, ProtocolErrorCode}, workspace_model::{ModelEdit, WorkspaceModel}};
use std::time::Instant;

fn unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, message, false)
}

/// Subscribe before reading: an independently committed execution/layout edit
/// is present in this snapshot or already queued as an edge. Duplicate older
/// edges are harmless; a commit can never fall into a subscribe/read gap.
pub(super) fn snapshot(host: &SharedHost, watcher: Option<ClientId>) -> Result<WorkspaceModel, ProtocolError> {
    let writer = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let writer = state.workspace_registry.clone().ok_or_else(|| unavailable("The workspace model writer is unavailable"))?;
        if let Some(watcher) = watcher { state.subscribe_model(watcher); }
        writer
    };
    let registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    registry.model().map_err(unavailable)
}

pub(super) fn edit(host: &SharedHost, origin: ClientId, edit: ModelEdit) -> Result<WorkspaceModel, ProtocolError> {
    let edit = edit.canonicalized().map_err(|error| ProtocolError::new(ProtocolErrorCode::InvalidRequest, error, false))?;
    let writer = host.lock().unwrap_or_else(|p| p.into_inner()).workspace_registry.clone()
        .ok_or_else(|| unavailable("The workspace model writer is unavailable"))?;
    // Keep this guard through publication so two concurrent clients cannot
    // publish sequence N+1 before N. No HostState operation acquires this mutex.
    let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    if let ModelEdit::ChangeView { scope, expected_revision, operation, .. } = &edit {
        if !registry.view_operation_seen(*operation).map_err(unavailable)?
            && registry.view_revision(scope).map_err(unavailable)? != *expected_revision {
            return Err(ProtocolError::new(ProtocolErrorCode::ViewConflict,
                "The workspace view changed; rebase this pane mutation on the current revision", true));
        }
    }
    let applied = registry.apply(&edit).map_err(|error| {
        if error == "workspace view revision conflict" {
            ProtocolError::new(ProtocolErrorCode::ViewConflict, error, true)
        } else { unavailable(error) }
    })?;
    if applied.changed {
        let waiting = Instant::now();
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let acquired = Instant::now();
        state.announce_model_edit(origin, applied.sequence, &edit);
        drop(state);
        tracing::debug!(operation = "workspace-model-edit", sequence = applied.sequence,
            host_lock_wait_us = acquired.duration_since(waiting).as_micros() as u64,
            host_lock_hold_us = acquired.elapsed().as_micros() as u64,
            "Published a workspace model edit");
        crate::daemon::wake();
    }
    Ok(applied.model)
}

/// Completing an automatic terminal reservation is a layout write too. It
/// follows the same writer/publication order as requests from window clients.
pub(super) fn complete_terminal(host: &SharedHost, terminal_id: crate::model::TerminalId) -> Result<(), String> {
    let (session, writer) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let session = state.terminals.get(&terminal_id).ok_or("terminal is no longer hosted")?.session.clone();
        (session, state.workspace_registry.clone().ok_or("workspace writer is unavailable")?)
    };
    let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    for view in registry.model()?.views {
        for before in view.panes {
            if before.pending_terminal_start
                && matches!(&before.kind, crate::model::CardKind::Terminal(terminal) if terminal.id == terminal_id) {
                let mut pane = before.clone();
                pane.pending_terminal_start = false;
                pane.kind = crate::model::CardKind::Terminal(session.clone());
                let edit = ModelEdit::ChangeView {
                    scope: view.scope.clone(), expected_revision: registry.view_revision(&view.scope)?,
                    operation: uuid::Uuid::new_v4(),
                    change: crate::workspace_model::views::Change::Update { before, pane },
                };
                let applied = registry.apply(&edit)?;
                if applied.changed {
                    host.lock().unwrap_or_else(|p| p.into_inner())
                        .announce_model_edit(0, applied.sequence, &edit);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host::{model::Registry, state::{HostLock, HostState}}, model::{BootId, HostIncarnation, HostSessionId, HostSessionProof}};
    use std::{sync::{Arc, mpsc}, thread, time::Duration};

    #[test]
    fn sqlite_writer_contention_does_not_hold_the_global_host_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let registry = Registry::at(&path).unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("workspace-lock"),
            boot_id: BootId::new("fixture"), host_incarnation: HostIncarnation::new("fixture"),
        });
        state.install_model(registry);
        let host = Arc::new(HostLock::new(state));
        let blocker = rusqlite::Connection::open(path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let edit_host = host.clone();
        let repository = directory.path().to_owned();
        let (started, ready) = mpsc::channel();
        let writer = thread::spawn(move || {
            started.send(()).unwrap();
            super::super::client::process_request(&edit_host, 1, crate::terminal_ipc::ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::OpenProject { repository, name: "fixture".into(), kind: Default::default() },
            })
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::sleep(Duration::from_millis(100));
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
        assert!(responsive, "SQLite writer contention blocked terminal delivery's host lock");
        assert_eq!(host.lock().unwrap().read_model().unwrap().projects.len(), 1);
    }
}
