//! Actual daemon admission for the removal-only worktree service.
//! Git and actor waits run outside HostLock; cleanup releases global admission.
use super::{
    state::{ClientId, SharedHost},
    worktree_inspections, worktree_sessions,
};
use crate::{
    terminal_ipc::{
        ClientRequest, ProtocolError, ProtocolErrorCode, ServerMessage, ServerResponse,
    },
    worktree_control::{Operation, Reply},
};

fn invalid(message: String) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

pub(super) fn process(
    host: &SharedHost,
    client: ClientId,
    operation: Operation,
) -> Result<(ServerResponse, Vec<ServerMessage>, bool), ProtocolError> {
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let response = |reply| {
        (
            ServerResponse::DeviceOperation {
                reply: crate::device_service::Reply::Worktree(reply),
            },
            Vec::new(),
            false,
        )
    };
    if let Operation::StopSessions {
        repository,
        path,
        consent,
        expected_fingerprint,
        stop_authority,
    } = &operation
    {
        let mut pending = {
            let _exclusive = gate.write().unwrap_or_else(|p| p.into_inner());
            super::client::ensure_control_open(host)?;
            super::agent_execution::terminals::admit(
                host,
                client,
                &ClientRequest::DeviceOperation {
                    operation: crate::device_service::Operation::Worktree {
                        operation: operation.clone(),
                    },
                },
            )?;
            worktree_sessions::prepare_inspected_stop(
                host,
                client,
                repository,
                path,
                consent,
                expected_fingerprint,
                *stop_authority,
            )
            .map_err(invalid)?
        };
        pending.cleanup().map_err(invalid)?;
        let _exclusive = gate.write().unwrap_or_else(|p| p.into_inner());
        super::client::ensure_control_open(host)?;
        return pending.commit(host).map(response).map_err(invalid);
    }
    let exclusive = operation.mutates();
    let _write = exclusive.then(|| gate.write().unwrap_or_else(|p| p.into_inner()));
    let _read = (!exclusive).then(|| gate.read().unwrap_or_else(|p| p.into_inner()));
    super::client::ensure_control_open(host)?;
    super::agent_execution::terminals::admit(
        host,
        client,
        &ClientRequest::DeviceOperation {
            operation: crate::device_service::Operation::Worktree {
                operation: operation.clone(),
            },
        },
    )?;
    let repository = operation.repository().map(std::path::Path::to_path_buf);
    let reply = match operation {
        operation @ Operation::Remove { .. } => {
            worktree_inspections::remove(host, client, operation)
        }
        operation @ Operation::DeleteBranch { .. } => {
            Ok(worktree_inspections::delete_branch(host, client, operation))
        }
        Operation::InspectForSessionStop { repository, path } => {
            worktree_inspections::inspect(host, client, &repository, &path)
        }
        Operation::BusySessions { repository, path } => {
            worktree_sessions::busy(host, &repository, &path)
        }
        Operation::CloseSessions {
            repository,
            path,
            agents,
            census,
        } => worktree_sessions::close(host, &repository, &path, &agents, &census),
        operation => crate::worktree_control::service::execute(operation, |path| {
            Ok(host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .worktree_live_sessions(path))
        })
        .map(|execution| execution.reply),
    }
    .map_err(invalid)?;
    if exclusive && matches!(reply, Reply::Removed { .. } | Reply::BranchDeleted { .. }) {
        if let Some(repository) = repository {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .announce_worktrees_changed(&repository);
        }
    }
    Ok(response(reply))
}
