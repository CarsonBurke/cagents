//! Canonical trusted setup preparation; actual launch belongs to the host.
use super::*;
pub(crate) struct PreparedSetup {
    pub(crate) custody: crate::repository::SetupCustody,
    pub(crate) recipe: Option<crate::model::LaunchRecipe>,
    pub(crate) result: Option<Setup>,
    pub(crate) warnings: Vec<String>,
}

pub(super) fn prepare<E: ProcessExecutor>(
    service: &Service<E>,
    store: &StateStore,
    request: &crate::repository::CreateWorktreeRequest,
    primary: &Path,
    plan: &crate::worktree_setup::SetupPlan,
    cancel: &CancellationToken,
) -> Result<PreparedSetup, String> {
    let intent = request
        .setup_intent
        .as_ref()
        .ok_or("Setup has no retained reviewed intent")?;
    let mut custody = service.worktree_setup_custody(store, request.operation_id, intent)?;
    let prepared = crate::worktree_setup::prepare_for_creation(
        primary,
        &request.destination,
        plan,
        &custody,
        cancel,
    )
    .map_err(|error| error.to_string())?;
    custody.copy_finished(service, store)?;
    let mut warnings = Vec::new();
    for pattern in prepared.missing_patterns {
        warnings.push(format!("No setup source matched {pattern}"));
    }
    for skipped in prepared.skipped_paths {
        warnings.push(format!(
            "Skipped copying {}: {}",
            skipped.path.display(),
            skipped.reason
        ));
    }
    let recipe = if prepared.command.is_some() {
        Some(custody.canonical_recipe()?)
    } else {
        None
    };
    if let Some(recipe) = &recipe {
        if prepared.command.as_deref() != recipe.arguments.get(1).map(String::as_str) {
            return Err("Prepared setup command differs from the exact retained plan".into());
        }
    }
    let result = recipe.as_ref().map(|recipe| Setup {
        command: recipe.arguments[1].clone(),
        run_id: custody.run,
        terminal: None,
        error: None,
    });
    Ok(PreparedSetup {
        custody,
        recipe,
        result,
        warnings,
    })
}
