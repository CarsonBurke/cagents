//! Management validates external authority before entering the registry. The
//! final registry transaction still checks revision and project membership.
use std::{fs::File, sync::Arc};

use uuid::Uuid;

use crate::{
    automations::{Automation, Definition, DirectoryIdentity},
    host::{agent_execution, state::SharedHost},
    process::SystemProcessExecutor,
};

use super::{schedule::Compiled, target};

/// Synchronous management entry: the host IPC dispatcher owns control.read().
/// No registry or HostState guard crosses target validation or actor cleanup.
pub(in crate::host) fn process_under_control(
    host: &SharedHost,
    authority: crate::agent_execution::Authority,
    request: crate::automations::Request,
) -> Result<crate::automations::Response, String> {
    use crate::automations::{Request, Response};
    if authority.token().is_some() {
        return Err("An agent's child-execution capability does not grant automation management. Use an explicit user session.".into());
    }
    let registry = agent_execution::registry(host)?;
    let handle = || {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .automations
            .clone()
            .ok_or_else(|| "The automation scheduler is unavailable".to_owned())
    };
    let response = match request {
        Request::List { after, limit } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automations(after, limit)
                .map(Response::Listed);
        }
        Request::Show { id } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation(id)
                .map(Response::Automation);
        }
        Request::History { id, before, limit } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_history(id, before, limit)
                .map(Response::History);
        }
        Request::ShowFiring { id } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_run_detail(id)
                .map(Response::Detail);
        }
        Request::ShowRun { run } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_detail_for_run(run)
                .map(Response::Detail);
        }
        Request::Create { definition } => Response::Automation(create(host, definition)?),
        Request::Edit {
            id,
            revision,
            definition,
        } => Response::Automation(edit(host, id, revision, definition)?),
        Request::SetEnabled {
            id,
            revision,
            enabled: value,
            confirmed,
        } => {
            if value {
                handle()?;
            }
            Response::Automation(enabled(host, id, revision, value, confirmed)?)
        }
        Request::Remove { id, revision } => {
            remove(host, id, revision)?;
            Response::Removed
        }
        Request::RunNow {
            id,
            execution_revision,
            request,
            confirmed,
        } => Response::Firing(handle()?.run_now(id, execution_revision, request, confirmed)?),
        Request::Cancel { id, firing } => {
            handle()?.cancel(id, firing)?;
            Response::CancellationRequested
        }
    };
    super::wake(host);
    Ok(response)
}

pub(in crate::host) fn create(
    host: &SharedHost,
    definition: Definition,
) -> Result<Automation, String> {
    let (definition, identity) = prepare(host, definition)?;
    agent_execution::registry(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .create_automation(definition, now()?, identity)
}

pub(super) fn edit(
    host: &SharedHost,
    id: Uuid,
    revision: u64,
    definition: Definition,
) -> Result<Automation, String> {
    let (definition, identity) = prepare(host, definition)?;
    let registry = agent_execution::registry(host)?;
    let previous = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .automation(id)?;
    let mut without_name = definition.clone();
    without_name.name = previous.definition.name.clone();
    let material = without_name != previous.definition
        || (definition == previous.definition && identity != previous.target_identity);
    let new_agent = definition.agent != previous.definition.agent
        || definition.session_mode != previous.definition.session_mode
        || (material && identity != previous.target_identity);
    if new_agent {
        retire_reuse_under_control(host, &previous, revision)?;
    }
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .edit_automation(id, revision, definition, now()?, identity)
}

pub(super) fn remove(host: &SharedHost, id: Uuid, revision: u64) -> Result<(), String> {
    let registry = agent_execution::registry(host)?;
    let automation = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .automation(id)?;
    retire_reuse_under_control(host, &automation, revision)?;
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove_automation(id, revision, now()?)
}

/// Management RPCs hold control.read(); the execution service additionally
/// checks the exact latest run under its per-agent gate before closing it.
fn retire_reuse_under_control(
    host: &SharedHost,
    automation: &Automation,
    revision: u64,
) -> Result<(), String> {
    if automation.revision != revision {
        return Err(
            "The automation changed before this action; review its current definition".into(),
        );
    }
    if automation.active_firing.is_some() || automation.blocked.is_some() {
        return Err(
            "Resolve the active or interrupted run before changing or removing this automation"
                .into(),
        );
    }
    let Some(agent) = automation.reuse_agent else {
        return Ok(());
    };
    let registry = agent_execution::registry(host)?;
    let run = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        if registry.agent(agent)?.closed {
            return Ok(());
        }
        registry.latest_agent_run(agent)?
    };
    match agent_execution::reconcile_agent_run_under_control(host, &run, true)? {
        agent_execution::Reconciliation::Closed => Ok(()),
        agent_execution::Reconciliation::Idle => Err("The provider did not confirm its close".into()),
        agent_execution::Reconciliation::UserOwned => Err("Interactive work owns this provider. Close it before changing or removing the automation.".into()),
    }
}

pub(super) fn enabled(
    host: &SharedHost,
    id: Uuid,
    revision: u64,
    enabled: bool,
    confirmed: bool,
) -> Result<Automation, String> {
    let registry = agent_execution::registry(host)?;
    let automation = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .automation(id)?;
    if automation.revision != revision {
        return Err(
            "The automation changed; review its current definition before enabling it".into(),
        );
    }
    let now = now()?;
    let next = if enabled {
        if !confirmed {
            return Err("Confirm unattended execution before enabling this automation".into());
        }
        validate_target(host, &automation.definition, &automation.target_identity)?;
        // Resolving an explicit saved account fails if that account was removed;
        // enabling never silently substitutes a project's new default account.
        agent_execution::selected_account(&automation.definition.agent)?;
        Some(Compiled::new(&automation.definition.schedule)?.next_after(now)?)
    } else {
        None
    };
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .set_automation_enabled(id, revision, next, confirmed, now)
}

fn prepare(
    host: &SharedHost,
    mut definition: Definition,
) -> Result<(Definition, DirectoryIdentity), String> {
    definition.validate()?;
    Compiled::new(&definition.schedule)?.next_after(now()?)?;
    let model = agent_execution::registry(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .model()?;
    definition.agent.target.repository =
        crate::project_location::resolve(&definition.agent.target.repository, Some(&model))?.key;
    definition.agent.target.directory =
        std::fs::canonicalize(&definition.agent.target.directory)
            .map_err(|error| format!("Cannot open the automation directory: {error}"))?;
    let identity =
        target::capture(&definition.agent.target.directory).map_err(|e| e.to_string())?;
    validate_target(host, &definition, &identity)?;
    definition.agent.account = Some(agent_execution::selected_account(&definition.agent)?.id());
    Ok((definition, identity))
}

/// The retained descriptor is passed to the execution service's actual spawn
/// boundary. Providers that re-resolve paths also validate it before commands.
pub(super) fn validate_target(
    host: &SharedHost,
    definition: &Definition,
    identity: &DirectoryIdentity,
) -> Result<Arc<File>, String> {
    let directory =
        target::verify(&definition.agent.target.directory, identity).map_err(|e| e.to_string())?;
    let kind = agent_execution::registry(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .automation_project_kind(&definition.agent.target.repository)?;
    target::project(
        &definition.agent.target.directory,
        &definition.agent.target.repository,
        kind,
        &SystemProcessExecutor,
    )?;
    // Detect replacement during discovery too; retain the original descriptor.
    target::verify(&definition.agent.target.directory, identity).map_err(|e| e.to_string())?;
    Ok(Arc::new(directory))
}

pub(super) fn now() -> Result<u64, String> {
    crate::chat::now_ms().ok_or_else(|| "The system clock is outside the supported range".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_execution::Authority,
        automations::Request,
        host::state::HostState,
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    };

    #[test]
    fn inherited_execution_authority_cannot_manage_automations() {
        let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
            HostSessionProof {
                host_session_id: HostSessionId::new("automation-authority-fixture"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        )));
        // No registry is installed: denial must precede even reading shared
        // automation definitions, rather than rely on a CLI-only restriction.
        let error = process_under_control(
            &host,
            Authority::session("private-child-capability".into()),
            Request::List {
                after: None,
                limit: 1,
            },
        )
        .unwrap_err();
        assert!(error.contains("does not grant automation management"));
        assert!(!error.contains("private-child-capability"));
    }
}
