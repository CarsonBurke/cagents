//! Explicit fresh review is the only way to replace a retained setup intent.
use super::*;
use crate::repository::{WorktreeOperationReport, WorktreeSetupIntent};

fn plan(plan: &crate::worktree_setup::SetupPlan) -> Plan {
    Plan {
        copy: plan.copy.clone(),
        commands: plan.commands.clone(),
        fingerprint: plan.fingerprint.clone(),
    }
}
fn evidence<E: ProcessExecutor>(
    service: &Service<E>,
    common: &Path,
    records: &[WorktreeRecord],
    report: &WorktreeOperationReport,
    cancellation: &CancellationToken,
) -> Result<SetupIntent, String> {
    let request = report
        .request
        .as_ref()
        .ok_or("Creation has no retained request")?;
    let intent = request
        .setup_intent
        .as_ref()
        .ok_or("Creation has no retained setup intent")?;
    if service
        .common_dir(&request.destination, cancellation)
        .map_err(|error| error.to_string())?
        != common
    {
        return Err("Setup belongs to another repository".into());
    }
    if !records.iter().any(|record| {
        record.path == request.destination
            && record.branch.as_deref() == Some(request.branch.full_ref().as_str())
            && !record.bare
    }) {
        return Err("The original setup checkout is no longer registered".into());
    }
    let admin = service
        .git_dir(&request.destination, cancellation)
        .map_err(|error| error.to_string())?;
    let admin = crate::fs_safety::Root::open(&admin).map_err(|error| error.to_string())?;
    if crate::repository::worktree_creation_marker(&admin).map_err(|error| error.to_string())?
        != report.operation_id
    {
        return Err("Setup has no matching original generation evidence".into());
    }
    Ok(SetupIntent {
        creation_id: report.operation_id,
        run_id: intent.run_id,
        path: request.destination.clone(),
        primary: intent
            .primary
            .clone()
            .ok_or("Setup has no retained primary checkout")?,
        retained_plan: intent.plan.as_ref().map(plan),
        skip: intent.skip,
        copy_done: intent.copy_done,
    })
}

pub(super) fn inventory<E: ProcessExecutor>(
    service: &Service<E>,
    store: &StateStore,
    repository: &Path,
    cancellation: &CancellationToken,
) -> Result<Reply, String> {
    let mut intents = Vec::new();
    let mut warnings = Vec::new();
    let common = service
        .common_dir(repository, cancellation)
        .map_err(|error| error.to_string())?;
    let records = service
        .list_worktrees(repository, cancellation)
        .map_err(|error| error.to_string())?;

    for row in store
        .journaled_worktree_operations()
        .map_err(|error| error.to_string())?
    {
        let (original_common, report) = match service.worktree_setup_creation_from_row(&row) {
            Ok(evidence) => evidence,
            Err(_) => continue, // Pending/legacy/non-created rows are not resume permission.
        };
        if original_common != common {
            continue;
        }
        let Some(request) = &report.request else {
            continue;
        };
        if request.setup_intent.is_none() {
            continue;
        }
        // Repository mismatch is ordinary inventory filtering. An unavailable
        // matching checkout is retained as a warning with its exact path.
        match evidence(service, &common, &records, &report, cancellation) {
            Ok(intent) => intents.push(intent),
            Err(error) => warnings.push(format!(
                "{} (creation {}): {error}",
                request.destination.display(),
                report.operation_id
            )),
        }
    }
    Ok(Reply::SetupIntents { intents, warnings })
}

fn retained<E: ProcessExecutor>(
    service: &Service<E>,
    store: &StateStore,
    repository: &Path,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<(WorktreeOperationReport, SetupIntent), String> {
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    let admin = service
        .git_dir(&path, cancellation)
        .map_err(|error| error.to_string())?;
    let admin = crate::fs_safety::Root::open(&admin).map_err(|error| error.to_string())?;
    let creation =
        crate::repository::worktree_creation_marker(&admin).map_err(|error| error.to_string())?;
    let report = service
        .worktree_setup_creation(store, creation)
        .map_err(|error| error.to_string())?
        .ok_or("Original setup journal is unavailable on this daemon; do not replay setup")?;
    let common = service
        .common_dir(repository, cancellation)
        .map_err(|error| error.to_string())?;
    let records = service
        .list_worktrees(repository, cancellation)
        .map_err(|error| error.to_string())?;
    let intent = evidence(service, &common, &records, &report, cancellation)?;
    if intent.path != path {
        return Err("Setup journal refers to another checkout".into());
    }
    Ok((report, intent))
}

pub(super) fn review<E: ProcessExecutor>(
    service: &Service<E>,
    store: &StateStore,
    repository: &Path,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<Reply, String> {
    let (_, intent) = retained(service, store, repository, path, cancellation)?;
    let (current_plan, trusted, current_error) = match crate::worktree_setup::load(&intent.primary)
    {
        Ok(Some(current)) => {
            match crate::worktree_setup::is_trusted(&intent.primary, &current, cancellation) {
                Ok(trusted) => (Some(plan(&current)), trusted, None),
                Err(error) => (Some(plan(&current)), false, Some(error.to_string())),
            }
        }
        Ok(None) => (None, true, None),
        Err(error) => (None, false, Some(error.to_string())),
    };
    Ok(Reply::SetupReview(SetupReview {
        intent,
        current_plan,
        trusted,
        current_error,
    }))
}

pub(super) fn resume<E: ProcessExecutor>(
    service: &Service<E>,
    store: &StateStore,
    repository: &Path,
    path: &Path,
    creation: OperationId,
    expected_run: Option<OperationId>,
    expected_fingerprint: Option<String>,
    trust: bool,
    skip: bool,
    mut live_sessions: impl FnMut(&Path) -> Result<Vec<crate::model::TerminalId>, String>,
    cancellation: &CancellationToken,
) -> Result<(Reply, Option<PreparedSetup>), String> {
    let (report, evidence) = retained(service, store, repository, path, cancellation)?;
    if evidence.creation_id != creation || evidence.run_id != expected_run {
        return Err("Setup run changed; review it again before resuming or skipping".into());
    }
    if !live_sessions(&evidence.path)?.is_empty() {
        return Err("Close the retained setup terminal and other live sessions before changing setup intent".into());
    }
    let mut request = report.request.ok_or("Setup request disappeared")?;
    let expected = request
        .setup_intent
        .as_ref()
        .ok_or("Setup intent disappeared")?;
    // Acquiring the original lease and disposition guard precedes trust/CAS.
    // Neither skip nor force can replace an unproved Reserved setup family.
    let mut custody = service.worktree_setup_transition_custody(store, creation, expected)?;
    if skip {
        // Skip binds the exact retained creation/run and original custody. It
        // neither reads nor trusts configuration, and performs no copy/shell
        // effects, so invalid or removed configuration cannot strand a checkout.
        let run_id = OperationId::new();
        let updated = WorktreeSetupIntent {
            primary: Some(evidence.primary.clone()),
            plan: expected.plan.clone(),
            skip: true,
            run_id: Some(run_id),
            copy_done: true,
        };
        custody.begin_reviewed_run(service, store, updated)?;
        return Ok((
            Reply::SetupPrepared {
                path: evidence.path,
                creation_id: creation,
                run_id,
                skip: true,
                setup: None,
                error: None,
            },
            None,
        ));
    }
    let current =
        crate::worktree_setup::load(&evidence.primary).map_err(|error| error.to_string())?;
    if current.as_ref().map(|plan| &plan.fingerprint) != expected_fingerprint.as_ref() {
        return Err(
            "Setup configuration changed; review its current commands and fingerprint again".into(),
        );
    }
    if !skip && let Some(plan) = &current {
        if trust {
            crate::worktree_setup::trust(&evidence.primary, plan, cancellation)
                .map_err(|error| error.to_string())?;
        }
        if !crate::worktree_setup::is_trusted(&evidence.primary, plan, cancellation)
            .map_err(|error| error.to_string())?
        {
            return Err("Review this exact setup plan and pass --trust before resuming it".into());
        }
    }
    let run_id = OperationId::new();
    let updated = WorktreeSetupIntent {
        primary: Some(evidence.primary.clone()),
        plan: current.clone(),
        skip,
        run_id: Some(run_id),
        copy_done: skip || current.is_none(),
    };
    custody.begin_reviewed_run(service, store, updated.clone())?;
    request.setup_intent = Some(updated);
    let mut reply = Reply::SetupPrepared {
        path: evidence.path,
        creation_id: creation,
        run_id,
        skip,
        setup: None,
        error: None,
    };
    if skip || current.is_none() {
        return Ok((reply, None));
    }
    match setup::prepare_with_custody(
        service,
        store,
        &request,
        &evidence.primary,
        current.as_ref().expect("present current plan"),
        custody,
        cancellation,
    ) {
        Ok(prepared) => {
            if let Reply::SetupPrepared { setup, .. } = &mut reply {
                *setup = prepared.result.clone();
            }
            Ok((reply, Some(prepared)))
        }
        Err(failure) => {
            if let Reply::SetupPrepared { error, .. } = &mut reply {
                *error = Some(failure);
            }
            Ok((reply, None))
        }
    }
}
