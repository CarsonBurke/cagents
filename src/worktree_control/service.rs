//! Blocking service work; no HostState borrow or control writer may be held.
mod recovery;
mod removal;
mod setup;
use super::*;
use crate::{
    model::OperationId,
    persistence::StateStore,
    repository::{
        CancellationToken, ProcessExecutor, RepositoryService, SystemProcessExecutor,
        TrackingState, WorktreeCreationOutcome, WorktreeInput, WorktreeOperationState,
        WorktreeRecord,
    },
};
pub(crate) use removal::PreparedRemoval;
pub(crate) use setup::PreparedSetup;
use std::{path::Path, sync::RwLock};
type Service<E = SystemProcessExecutor> = RepositoryService<E>;
// Reconciliation cannot settle a journal whose live creator has not acquired
// its process lease yet. All service creations share this existing boundary.
static OPERATIONS: RwLock<()> = RwLock::new(());

pub(crate) enum Execution {
    Reply {
        reply: Reply,
        setup: Option<PreparedSetup>,
    },
    /// The host installs its existing per-target Reservation before committing
    /// removal, then supplies its real occupancy/process checks to remove().
    Remove(PreparedRemoval),
}

pub(crate) fn execute<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    operation: Operation,
    store: &StateStore,
    live_sessions: impl FnMut(&Path) -> Result<Vec<crate::model::TerminalId>, String>,
) -> Result<Execution, String> {
    let settings = if matches!(operation, Operation::Create { .. }) {
        let path = crate::instance::settings_path().ok_or("Settings directory is unavailable")?;
        Some(crate::settings::load(path).map_err(|error| error.to_string())?)
    } else {
        None
    };
    execute_with(service, operation, store, live_sessions, settings)
}

fn execute_with<E: ProcessExecutor>(
    service: &Service<E>,
    operation: Operation,
    store: &StateStore,
    mut live_sessions: impl FnMut(&Path) -> Result<Vec<crate::model::TerminalId>, String>,
    settings: Option<crate::settings::AppSettings>,
) -> Result<Execution, String> {
    let cancellation = CancellationToken::new();
    // A read-only list must not create/migrate an unrelated window database.
    // The host supplies its own daemon journal, opened outside HostState.
    if matches!(operation, Operation::Create { .. }) {
        match OPERATIONS.try_write() {
            Ok(_idle) => {
                service
                    .rehydrate_pending_worktree_operations(store)
                    .map_err(|e| e.to_string())?;
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let _idle = error.into_inner();
                service
                    .rehydrate_pending_worktree_operations(store)
                    .map_err(|e| e.to_string())?;
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
    }
    let _active = OPERATIONS.read().unwrap_or_else(|error| error.into_inner());
    let (reply, setup) = match operation {
        Operation::ResolveProject { id } => (
            Reply::ResolvedRepository {
                repository: resolve_project(id)?,
            },
            None,
        ),
        Operation::ResolveRepository { path } => {
            let discovery = service
                .discover(&path, &cancellation)
                .map_err(|e| e.to_string())?;
            (
                Reply::ResolvedRepository {
                    repository: discovery.common_dir,
                },
                None,
            )
        }
        Operation::List { repository } => {
            let records = service
                .list_worktrees(&repository, &cancellation)
                .map_err(|e| e.to_string())?;
            let mut worktrees = Vec::new();
            for (index, record) in records
                .iter()
                .enumerate()
                .filter(|(_, record)| !record.bare)
            {
                let mut row = match entry(service, record, index == 0, &cancellation) {
                    Ok(row) => row,
                    Err(error) => Entry {
                        path: record.path.clone(),
                        branch: short_branch(record),
                        head_oid: record.head_oid.clone(),
                        primary: index == 0,
                        detached: record.detached,
                        locked: record.lock.locked,
                        dirty: None,
                        status_error: Some(error),
                        ahead: None,
                        behind: None,
                        live_sessions: None,
                        session_error: None,
                    },
                };
                match live_sessions(&record.path) {
                    Ok(sessions) => row.live_sessions = Some(sessions),
                    Err(error) => row.session_error = Some(error),
                }
                worktrees.push(row);
            }
            (Reply::Listed { worktrees }, None)
        }
        Operation::Create {
            repository,
            branch,
            base,
            linear,
            no_setup,
        } => {
            let linear = linear
                .map(|issue| crate::linear::issue_reference(&issue))
                .transpose()
                .map_err(|e| e.to_string())?;
            let primary = service
                .managed_primary_checkout(&repository, &cancellation)
                .map_err(|error| error.to_string())?
                .ok_or("A non-bare primary checkout is required")?;
            let plan = if no_setup {
                None
            } else {
                crate::worktree_setup::load(&primary).map_err(|e| e.to_string())?
            };
            if let Some(plan) = &plan {
                if plan.requires_trust()
                    && !crate::worktree_setup::is_trusted(&primary, plan, &cancellation)
                        .map_err(|e| e.to_string())?
                {
                    return Err("This project's setup commands have not been trusted. Review them in the worktree dialog, or pass --no-setup".into());
                }
            }
            let settings = settings.ok_or("Creation settings were not loaded before admission")?;
            let mut request = service
                .resolve_worktree_request(
                    &WorktreeInput {
                        repository,
                        input: branch,
                        base: base.unwrap_or_default(),
                        managed_directory: settings.managed_worktree_directory,
                        destination: String::new(),
                        operation_id: OperationId::new(),
                    },
                    &cancellation,
                )
                .map_err(|e| e.to_string())?;
            if plan.is_some() || no_setup {
                request.setup_intent = Some(crate::repository::WorktreeSetupIntent {
                    primary: Some(primary.clone()),
                    plan: plan.clone(),
                    skip: no_setup,
                    run_id: Some(OperationId::new()),
                    copy_done: no_setup,
                });
            }
            let report = service.create_worktree_journaled_sync(
                store,
                &request,
                linear.map(|issue_id| crate::linear::context::LinkedIssue {
                    issue_id,
                    workspace: None,
                }),
                0,
                &cancellation,
                |_| {},
            );
            let (record, warnings) = match report.outcome {
                Some(WorktreeCreationOutcome::Created { worktree, warnings })
                    if report.state == WorktreeOperationState::Created =>
                {
                    (
                        worktree,
                        warnings
                            .into_iter()
                            .map(|warning| warning.reported_detail())
                            .collect(),
                    )
                }
                outcome => {
                    let residual = match outcome {
                        Some(WorktreeCreationOutcome::OutcomeUnknown { residual, .. }) => {
                            serde_json::to_value(residual).ok()
                        }
                        Some(WorktreeCreationOutcome::Created { worktree, .. }) => {
                            Some(serde_json::json!({"registered_worktree":worktree}))
                        }
                        _ => None,
                    };
                    let may_have_effects = residual.is_some()
                        || !matches!(
                            report.state,
                            WorktreeOperationState::NotCreated
                                | WorktreeOperationState::MalformedPayload
                                | WorktreeOperationState::LeaseConflict
                        );
                    return Ok(Execution::Reply {
                        reply: Reply::CreationFailed(CreationFailure {
                            operation_id: request.operation_id,
                            path: request.destination.clone(),
                            state: serde_json::to_value(report.state)
                                .ok()
                                .and_then(|state| state.as_str().map(str::to_owned))
                                .unwrap_or_else(|| "unknown".into()),
                            residual,
                            error: report.detail,
                            may_have_effects,
                        }),
                        setup: None,
                    });
                }
            };
            let mut created = Created {
                operation_id: request.operation_id,
                path: record.path.clone(),
                branch: short_branch(&record),
                worktree: None,
                warnings,
                setup: None,
                error: None,
            };
            // All failures after settlement remain typed committed outcomes.
            // The host can still publish this creation's captured path, and
            // the CLI reports nonzero with the exact retained receipt.
            match entry(service, &record, false, &cancellation) {
                Ok(row) => created.worktree = Some(row),
                Err(error) => {
                    created.error = Some(format!(
                        "Created checkout retained, but its status could not be read: {error}"
                    ))
                }
            }
            let prepared = match plan {
                Some(plan) => {
                    match setup::prepare(service, store, &request, &primary, &plan, &cancellation) {
                        Ok(prepared) => {
                            created.warnings.extend(prepared.warnings.clone());
                            created.setup = prepared.result.clone();
                            Some(prepared)
                        }
                        Err(error) => {
                            created.setup = Some(Setup {
                                command: String::new(),
                                run_id: request
                                    .setup_intent
                                    .as_ref()
                                    .and_then(|intent| intent.run_id)
                                    .expect("new retained run"),
                                terminal: None,
                                attempted_terminal: None,
                                error: Some(error),
                            });
                            None
                        }
                    }
                }
                None => None,
            };
            (Reply::Created(created), prepared)
        }
        Operation::SetupIntents { repository } => (
            recovery::inventory(service, store, &repository, &cancellation)?,
            None,
        ),
        Operation::ReviewSetup { repository, path } => (
            recovery::review(service, store, &repository, &path, &cancellation)?,
            None,
        ),
        Operation::ResumeSetup {
            repository,
            path,
            creation_id,
            expected_run,
            expected_fingerprint,
            trust,
            skip,
        } => recovery::resume(
            service,
            store,
            &repository,
            &path,
            creation_id,
            expected_run,
            expected_fingerprint,
            trust,
            skip,
            &mut live_sessions,
            &cancellation,
        )?,
        Operation::Remove {
            repository,
            target,
            force,
            delete_branch,
            resolved_path,
        } => {
            return PreparedRemoval::prepare(
                service,
                repository,
                target,
                resolved_path,
                force,
                delete_branch,
                &cancellation,
            )
            .map(Execution::Remove);
        }
    };
    Ok(Execution::Reply { reply, setup })
}

fn short_branch(record: &WorktreeRecord) -> Option<String> {
    record.branch.as_deref().map(|branch| {
        branch
            .strip_prefix("refs/heads/")
            .unwrap_or(branch)
            .to_owned()
    })
}
fn entry<E: ProcessExecutor>(
    service: &Service<E>,
    record: &WorktreeRecord,
    primary: bool,
    cancellation: &CancellationToken,
) -> Result<Entry, String> {
    let status = service
        .status(&record.path, cancellation)
        .map_err(|e| e.to_string())?;
    let (ahead, behind) = match status.tracking {
        TrackingState::Compared { ahead, behind, .. } => (Some(ahead), Some(behind)),
        _ => (None, None),
    };
    Ok(Entry {
        path: record.path.clone(),
        branch: short_branch(record),
        head_oid: record.head_oid.clone(),
        primary,
        detached: record.detached,
        locked: record.lock.locked,
        dirty: Some(status.dirty.is_dirty()),
        status_error: None,
        ahead,
        behind,
        live_sessions: None,
        session_error: None,
    })
}

fn resolve_project(id: crate::model::ProjectId) -> Result<PathBuf, String> {
    use rusqlite::OptionalExtension;
    let mut matches = std::collections::BTreeSet::new();
    for path in crate::instance::window_database_paths().map_err(|e| e.to_string())? {
        if !path.try_exists().map_err(|e| e.to_string())? {
            continue;
        }
        let connection = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|e| e.to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        let row: Option<(String, String)> = connection
            .query_row(
                "SELECT repository_json,existence_json FROM projects WHERE project_id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((repository, existence)) = row {
            let existence: crate::model::Existence =
                serde_json::from_str(&existence).map_err(|e| e.to_string())?;
            if existence.is_open() {
                matches.insert(
                    serde_json::from_str::<PathBuf>(&repository).map_err(|e| e.to_string())?,
                );
            }
        }
    }
    if matches.len() != 1 {
        return Err(format!(
            "Project {id} is unknown, closed or ambiguous; use its absolute checkout path"
        ));
    }
    Ok(matches.into_iter().next().expect("one project"))
}

#[cfg(test)]
mod tests;
