//! The standalone shell uses the same durable worktree journal as the ADE.
//! UI state owns cancellation; dropping a review never erases an accepted job.

use crate::{
    persistence::StateStore,
    repository::{
        CancellationToken, CreateWorktreeRequest, RepositoryService, WorktreeOperationReport,
    },
};

/// Executes an already confirmed immutable request. The journal records every
/// effect before Git starts and reconciles an interrupted checkout on reopen.
pub fn create(
    store: &StateStore,
    request: &CreateWorktreeRequest,
    cancellation: &CancellationToken,
) -> WorktreeOperationReport {
    create_with_service(&RepositoryService::system("git"), store, request, cancellation)
}

/// Reuses the durable journal with a caller's retained repository authority.
/// The service retains responsibility for validating every Git scope.
pub fn create_with_service<E: crate::process::ProcessExecutor>(
    service: &RepositoryService<E>,
    store: &StateStore,
    request: &CreateWorktreeRequest,
    cancellation: &CancellationToken,
) -> WorktreeOperationReport {
    let (progress, _receiver) = async_channel::bounded(8);
    service.create_worktree_journaled(
        store,
        request,
        1,
        cancellation,
        &progress,
    )
}

/// A scoped creator must return its ORIGINAL live session on every outcome.
/// Callers retain this carrier before inspecting its descriptive path report.
pub fn create_retained_with_service<E: crate::process::ProcessExecutor>(
    service: &RepositoryService<E>,
    store: &StateStore,
    request: &CreateWorktreeRequest,
    cancellation: &CancellationToken,
) -> crate::repository::RetainedWorktreeCreation {
    let (progress, _receiver) = async_channel::bounded(8);
    service.create_worktree_journaled_retained(store, request, 1, cancellation, &progress)
}
