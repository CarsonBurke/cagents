//! A device window asks once. Its daemon resolves ownership and closes exact
//! resident sources without making the displaying computer attach to them.
use super::{ClientRequest, ProtocolError, ProtocolErrorCode, ServerResponse, SharedHost};
use crate::{
    device_service::{Reply, SessionCloseScope},
    host::{chat, state::ClientId, workspace},
    model::{ProjectKind, TerminalContinuity},
    project_sessions::{
        ProjectSessionClosure, ProjectSessionOwnership, ProjectSessionScope, add_saved_owners,
        shared_project_session_scope,
    },
    repository::{CancellationToken, RepositoryService},
    terminal_ipc::TerminalCloseResult,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

enum ChatTarget {
    Hosted(Result<chat::CloseTarget, ProtocolError>),
    CleanupAttachment(uuid::Uuid),
    Starting,
}

pub(super) fn close(
    host: &SharedHost,
    client_id: ClientId,
    request: SessionCloseScope,
    saved_windows: impl FnOnce() -> std::io::Result<Vec<PathBuf>>,
) -> Result<Reply, ProtocolError> {
    // Don't start another close on behalf of a request whose client is about
    // to time out. An already admitted teardown can still finish.
    let deadline =
        Instant::now() + crate::control::DEVICE_OPERATION_DEADLINE - Duration::from_secs(5);
    match request {
        SessionCloseScope::Project {
            repository,
            kind,
            roots,
            ownership,
            only_if_closed,
        } => {
            let closure =
                project_closure(host, &repository, kind, roots, ownership, saved_windows)?;
            // Ensure the success reply can be sent before closing anything.
            if !postcard::experimental::serialized_size(&closure)
                .is_ok_and(|size| size < crate::terminal_ipc::MAX_FRAME_BYTES - 4096)
            {
                return Err(refusal(
                    "The device's project ownership is too large to return",
                ));
            }
            close_owned(host, client_id, Some(&closure), &closure.scope, || {
                check_close_open(host, deadline)?;
                if only_if_closed && project_is_open(host, &repository)? {
                    return Err(refusal(
                        "The project was reopened before session cleanup finished.",
                    ));
                }
                Ok(())
            })?;
            Ok(Reply::SessionsClosed(Some(closure)))
        }
        SessionCloseScope::Worktree { path } => {
            if !path.is_absolute() {
                return Err(refusal("The worktree path must be absolute"));
            }
            let scope = ProjectSessionScope::new(vec![path], Vec::new());
            close_owned(host, client_id, None, &scope, || {
                check_close_open(host, deadline)
            })?;
            Ok(Reply::SessionsClosed(None))
        }
    }
}

fn project_closure(
    host: &SharedHost,
    repository: &Path,
    kind: ProjectKind,
    mut roots: Vec<PathBuf>,
    mut ownership: ProjectSessionOwnership,
    saved_windows: impl FnOnce() -> std::io::Result<Vec<PathBuf>>,
) -> Result<ProjectSessionClosure, ProtocolError> {
    let registry = workspace::writer(host)?;
    let projects = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .projects()
        .map_err(refusal)?;
    let git = RepositoryService::system("git");
    let cancellation = CancellationToken::default();
    let worktrees = |repository: &Path| {
        git.list_worktrees(repository, &cancellation)
            .map(|records| {
                records
                    .into_iter()
                    .map(|record| record.path)
                    .collect::<Vec<_>>()
            })
            .map_err(|error| error.to_string())
    };
    // A removed or missing checkout still has sessions at its saved paths.
    if kind == ProjectKind::Git
        && let Ok(current) = worktrees(repository)
    {
        roots.extend(current);
        roots.sort();
        roots.dedup();
    }
    let scope =
        shared_project_session_scope(&roots, repository, &projects, worktrees).map_err(refusal)?;
    add_saved_owners(&mut ownership, repository, saved_windows()).map_err(refusal)?;
    Ok(ProjectSessionClosure { scope, ownership })
}

fn close_owned(
    host: &SharedHost,
    client_id: ClientId,
    project: Option<&ProjectSessionClosure>,
    scope: &ProjectSessionScope,
    check: impl Fn() -> Result<(), ProtocolError>,
) -> Result<(), ProtocolError> {
    let owns = |id, directory: &Path| match project {
        Some(closure) => closure.ownership.contains(id, directory, scope),
        None => scope.contains(directory),
    };
    check()?;
    let gate = super::control_gate(host);
    let sessions = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        chat::chat_list_inventory(&state)
            .into_iter()
            .map(|session| {
                let target = match state.chats.get(&session.id) {
                    Some(record) if record.handle.is_close_only() => {
                        ChatTarget::CleanupAttachment(record.execution_instance)
                    }
                    Some(record) => ChatTarget::Hosted(chat::CloseTarget::for_execution(
                        &state, record, &session,
                    )),
                    None => ChatTarget::Starting,
                };
                (session, target)
            })
            .collect::<Vec<_>>()
    };
    for (session, target) in sessions {
        if !owns(session.id, &session.launch.recipe.working_directory)
            || project.is_some()
                && session
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| !owns(terminal.id, &terminal.launch.working_directory))
        {
            continue;
        }
        let _shared = gate.shared();
        check()?;
        let target = match target {
            ChatTarget::Hosted(target) => target?,
            ChatTarget::CleanupAttachment(instance) => {
                let still_exact = host
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .chats
                    .get(&session.id)
                    .is_some_and(|record| record.execution_instance == instance);
                if !still_exact {
                    return Err(refusal(
                        "The conversation changed before it could be closed. Retry.",
                    ));
                }
                let target = chat::CloseTarget::recover_for_close(host, &session, instance)?;
                check()?;
                target
            }
            ChatTarget::Starting => {
                // A pending actor can outlive its starting window. Its absence
                // from the hosted records is no confirmation that it stopped.
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.chat_spawning.contains_key(&session.id)
                    || state.chats.contains_key(&session.id)
                {
                    return Err(refusal(
                        "A conversation is still starting. Retry once it finishes.",
                    ));
                }
                continue;
            }
        };
        if let Err(error) = chat::close_captured(host, target)
            && host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .chats
                .contains_key(&session.id)
        {
            return Err(error);
        }
    }
    // Closing native actors can retire their PTYs. Inventory them after the
    // actor sweep and fence each terminal close to the observed worker.
    let terminals = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .list(client_id)
            .into_iter()
            .map(|terminal| {
                let session = terminal.session;
                let basis = state.terminal_removal_basis(&session);
                let paired = state
                    .chats
                    .get(&session.id)
                    .map(|record| record.current_session());
                (session, basis, paired)
            })
            .collect::<Vec<_>>()
    };
    for (session, basis, paired) in terminals {
        if !owns(session.id, &session.launch.working_directory) {
            continue;
        }
        if project.is_some()
            && paired.is_some_and(|chat| !owns(chat.id, &chat.launch.recipe.working_directory))
        {
            continue;
        }
        let shared = gate.shared();
        check()?;
        let basis = basis?;
        if matches!(
            basis,
            crate::workspace_model::views::TerminalRemovalBasis::Absent { .. }
        ) {
            continue;
        }
        let request = ClientRequest::CloseTerminalSource {
            completed_only: matches!(
                session.continuity,
                TerminalContinuity::Exited { .. }
                    | TerminalContinuity::RelaunchAvailable {
                        last_exit: Some(_),
                        ..
                    }
            ),
            allow_active: true,
            capture: None,
            session: session.clone(),
            basis,
        };
        let result = super::process_admitted_request(host, client_id, request, Some(shared), true);
        let error = match result {
            Ok((
                ServerResponse::TerminalCloseChecked {
                    outcome: TerminalCloseResult::Closed,
                    ..
                },
                _,
                _,
            )) => continue,
            Ok(_) => refusal("The device could not confirm that a terminal stopped"),
            Err(error) => error,
        };
        if host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
            .contains_key(&session.id)
        {
            return Err(error);
        }
    }
    Ok(())
}

fn project_is_open(host: &SharedHost, repository: &Path) -> Result<bool, ProtocolError> {
    workspace::writer(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .project_is_open(repository)
        .map_err(refusal)
}

fn check_close_open(host: &SharedHost, deadline: Instant) -> Result<(), ProtocolError> {
    if Instant::now() >= deadline {
        return Err(refusal(
            "Closing the sessions took too long. Retry to close the remaining sessions.",
        ));
    }
    super::ensure_control_open(host)?;
    super::ensure_terminal_close_open(host)
}

fn refusal(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
