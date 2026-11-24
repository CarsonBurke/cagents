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

#[derive(Debug, thiserror::Error)]
pub(in crate::host) enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    PermissionDenied(String),
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}
impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::Invalid(message.into())
    }
}
impl From<crate::host::model::AuthorityError> for Error {
    fn from(error: crate::host::model::AuthorityError) -> Self {
        match error {
            crate::host::model::AuthorityError::Denied(message) => Self::PermissionDenied(message),
            crate::host::model::AuthorityError::Failed(message) => Self::Invalid(message),
        }
    }
}

impl Error {
    pub(in crate::host) fn into_protocol(self) -> crate::terminal_ipc::ProtocolError {
        use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
        let (code, message) = match self {
            Self::Invalid(message) => (ProtocolErrorCode::InvalidRequest, message),
            Self::PermissionDenied(message) => (ProtocolErrorCode::PermissionDenied, message),
        };
        ProtocolError::new(code, message, false)
    }
}

/// Synchronous management entry: the host IPC dispatcher holds shared control
/// admission.
/// No registry or HostState guard crosses target validation or actor cleanup.
pub(in crate::host) fn process_under_control(
    host: &SharedHost,
    authority: crate::agent_execution::Authority,
    request: crate::automations::Request,
) -> Result<crate::automations::Response, Error> {
    if authority.token().is_some() && !request.allowed_from_hosted_conversation() {
        return Err(Error::PermissionDenied("An agent's child-execution capability does not grant automation management. Prepare a proposal for explicit user approval.".into()));
    }
    if authority.token().is_some() {
        return process_hosted(host, &authority, request);
    }
    process_user(host, request).map_err(Error::Invalid)
}

fn process_user(
    host: &SharedHost,
    request: crate::automations::Request,
) -> Result<crate::automations::Response, String> {
    use crate::automations::{Request, Response};
    let registry = agent_execution::registry(host)?;
    let handle = || {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .automations
            .clone()
            .ok_or_else(|| "The automation scheduler is unavailable".to_owned())
    };
    let response = match request {
        Request::Propose { .. } => {
            return Err(
                "Prepare automation approval requests from a live hosted conversation".into(),
            );
        }
        Request::Proposal { id } => {
            return Ok(Response::Proposal(Box::new(
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .automation_proposal(id, None)?
                    .ok_or("The automation proposal is unavailable")?,
            )));
        }
        Request::PendingProposals {
            repository,
            after,
            limit,
        } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pending_automation_proposals(&repository, after, limit)
                .map(Response::Proposals);
        }
        Request::ResolveProposal {
            id,
            revision,
            approved,
        } => {
            let proposal = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_proposal(id, None)?
                .ok_or("The automation proposal is unavailable")?;
            if proposal.revision != revision {
                return Err(
                    "The automation proposal changed; review its current definition".into(),
                );
            }
            if proposal.state != crate::automations::ProposalState::Pending {
                return Ok(Response::Proposal(Box::new(proposal)));
            }
            let _directory = if approved {
                Some(validate_target(
                    host,
                    &proposal.definition,
                    &proposal.target_identity,
                )?)
            } else {
                None
            };
            let next = if approved {
                handle()?;
                agent_execution::selected_account(&proposal.definition.agent)?;
                Some(Compiled::new(&proposal.definition.schedule)?.next_after(now()?)?)
            } else {
                None
            };
            let resolved = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .resolve_automation_proposal(&proposal, next, now()?)?;
            proposals_changed(host, &resolved.definition.agent.target.repository);
            Response::Proposal(Box::new(resolved))
        }
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
                .map(Box::new)
                .map(Response::Automation);
        }
        Request::History { id, before, limit } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_history(id, before, limit)
                .map(Response::History);
        }
        Request::Audit { id, before, limit } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_audit(id, before, limit)
                .map(Response::Audit);
        }
        Request::ShowFiring { id } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_run_detail(id)
                .map(|detail| Response::Detail(Box::new(detail)));
        }
        Request::ShowRun { run } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_detail_for_run(run)
                .map(|detail| Response::Detail(Box::new(detail)));
        }
        Request::Create { definition } => Response::Automation(Box::new(create(host, definition)?)),
        Request::Edit {
            id,
            revision,
            definition,
        } => {
            let automation = edit(host, id, revision, definition)?;
            attention_changed(host, id);
            Response::Automation(Box::new(automation))
        }
        Request::SetEnabled {
            id,
            revision,
            enabled: value,
            confirmed,
        } => {
            if value {
                handle()?;
            }
            let automation = enabled(host, id, revision, value, confirmed)?;
            if automation.enabled {
                attention_changed(host, id);
            }
            Response::Automation(Box::new(automation))
        }
        Request::Remove { id, revision } => {
            remove(host, id, revision)?;
            attention_changed(host, id);
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

fn attention_changed(host: &SharedHost, id: Uuid) {
    if let Some(notifications) = &host.lock().unwrap_or_else(|p| p.into_inner()).notifications {
        notifications.automation_attention_changed(id);
    }
}

/// Inspection and proposal ownership follow authenticated session lineage.
/// A plain token, stale session or delegated child never gains scheduler
/// mutation authority through this path.
fn process_hosted(
    host: &SharedHost,
    authority: &crate::agent_execution::Authority,
    request: crate::automations::Request,
) -> Result<crate::automations::Response, Error> {
    use crate::automations::{Request, Response};
    let (owner, source) = agent_execution::checked_caller(host, authority)?;
    let source = source
        .ok_or_else(|| Error::PermissionDenied("A live hosted conversation is required".into()))?;
    let registry = agent_execution::registry(host)?;
    // Waiters need only their lineage's immutable receipt. Avoid loading the
    // project model on every poll, and let a retry recover its saved decision
    // without depending on later project or account defaults.
    match &request {
        Request::Proposal { id } => {
            let proposal = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_proposal(*id, Some(&owner))?
                .ok_or_else(|| {
                    Error::PermissionDenied(
                        "This conversation has no such automation proposal".into(),
                    )
                })?;
            return Ok(Response::Proposal(Box::new(proposal)));
        }
        Request::Propose {
            request,
            definition,
        } => {
            if let Some(previous) = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_proposal(*request, Some(&owner))?
            {
                if previous.requested != *definition {
                    return Err(
                        "This request ID belongs to another saved definition; use a new request ID"
                            .into(),
                    );
                }
                return Ok(Response::Proposal(Box::new(previous)));
            }
        }
        _ => {}
    }
    let directory = {
        let host = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(chat) = host.chats.get(&source) {
            chat.session.launch.recipe.working_directory.clone()
        } else {
            host.terminals
                .get(&source)
                .ok_or_else(|| {
                    Error::PermissionDenied("The requesting session is no longer live".into())
                })?
                .session
                .launch
                .working_directory
                .clone()
        }
    };
    let model = registry.lock().unwrap_or_else(|p| p.into_inner()).model()?;
    let repository = crate::project_location::place(&directory, &model)
        .project
        .ok_or("The requesting conversation has no open project")?;
    let require_scope = |target: &std::path::Path| {
        if target == repository {
            Ok(())
        } else {
            Err(Error::PermissionDenied("Hosted conversations can inspect and propose automations only in their own project".into()))
        }
    };
    match request {
        Request::Propose { request, definition } => {
            let requested = definition.clone();
            let project = crate::project_location::resolve(
                &definition.agent.target.repository, Some(&model),
            )?.key;
            require_scope(&project)?;
            let (definition, identity) = prepare(host, definition)?;
            require_scope(&definition.agent.target.repository)?;
            let channel_title = definition.agent.target.channel().map(|channel| {
                let channel = model.channel(channel).ok_or("The selected channel is unavailable")?;
                require_scope(&channel.repository)?;
                Ok::<_, Error>(channel.title.clone())
            }).transpose()?;
            let proposal = registry.lock().unwrap_or_else(|p| p.into_inner())
                .propose_automation(request, owner, requested, definition, identity, channel_title, now()?)?;
            proposals_changed(host, &proposal.definition.agent.target.repository);
            Ok(Response::Proposal(Box::new(proposal)))
        }
        Request::List { after, limit } => registry.lock().unwrap_or_else(|p| p.into_inner())
            .project_automations(&repository, after, limit).map(Response::Listed).map_err(Into::into),
        Request::Show { id } => {
            let automation = registry.lock().unwrap_or_else(|p| p.into_inner()).automation(id)?;
            require_scope(&automation.definition.agent.target.repository)?;
            Ok(Response::Automation(Box::new(automation)))
        }
        Request::History { id: Some(id), before, limit } => registry.lock().unwrap_or_else(|p| p.into_inner())
            .project_automation_history(&repository, id, before, limit).map(Response::History).map_err(Into::into),
        Request::Audit { id, before, limit } => {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            registry.project_automation_audit(&repository, id, before, limit).map(Response::Audit).map_err(Into::into)
        }
        Request::ShowFiring { id } => {
            let detail = registry.lock().unwrap_or_else(|p| p.into_inner()).automation_run_detail(id)?;
            require_scope(&detail.definition.agent.target.repository)?;
            Ok(Response::Detail(Box::new(detail)))
        }
        Request::ShowRun { run } => {
            let detail = registry.lock().unwrap_or_else(|p| p.into_inner()).automation_detail_for_run(run)?;
            require_scope(&detail.definition.agent.target.repository)?;
            Ok(Response::Detail(Box::new(detail)))
        }
        Request::History { id: None, .. } => Err("Choose an automation ID for scoped run history".into()),
        _ => Err(Error::PermissionDenied("An agent's child-execution capability does not grant automation management. Prepare a proposal for explicit user approval.".into())),
    }
}

/// Windows offer a project's pending automation requests, and read them
/// again when told they changed rather than polling the registry.
fn proposals_changed(host: &SharedHost, repository: &std::path::Path) {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .automation_proposals_changed(repository);
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

/// Management RPCs hold shared control admission; the execution service additionally
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
        agent_execution::Reconciliation::Busy => Err("The provider is still busy; wait until it is idle before changing or removing the automation.".into()),
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
        automation.definition.startable()?;
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
    // Prechecks run here directly; accepted work is refused again at launch.
    crate::host::worktree_removal::ensure_outside_directory(host, &directory)
        .map_err(|error| error.message)?;
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
            Request::ResolveProposal {
                id: Uuid::new_v4(),
                revision: 1,
                approved: true,
            },
        )
        .unwrap_err();
        assert!(matches!(&error, Error::PermissionDenied(_)));
        assert!(
            error
                .to_string()
                .contains("does not grant automation management")
        );
        assert!(!error.to_string().contains("private-child-capability"));
    }

    #[test]
    fn enabling_refuses_a_saved_permission_mode_its_provider_lacks() {
        use crate::{model::ProjectKind, workspace_model::ModelEdit};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("registry.sqlite3");
        let mut registry = crate::host::model::Registry::at(&path).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: root.path().to_owned(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let definition = serde_json::from_value(serde_json::json!({
            "name":"Plan check", "prompt":"Fixture prompt",
            "agent":{"provider":"Antigravity","account":null,"target":{"repository":root.path(),"directory":root.path(),"channel":null},"surface":"Chat","model":null,"permission_mode":"plan"},
            "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},
            "session_mode":"fresh","precheck":null
        }))
        .unwrap();
        let automation = registry
            .create_automation(
                definition,
                100,
                crate::automations::DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        // Saved before each provider's permission modes were checked.
        let changed = rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE automations SET record=replace(record, '\"permission_mode\":\"plan\"', '\"permission_mode\":\"never\"') WHERE id=?1",
                [automation.id.to_string()],
            )
            .unwrap();
        assert_eq!(changed, 1);
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("automation-mode-fixture"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        });
        state.install_model(registry);
        let host = Arc::new(crate::host::state::HostLock::new(state));
        assert_eq!(
            enabled(&host, automation.id, automation.revision, true, true).unwrap_err(),
            "Antigravity has no permission mode \"never\"; choose default, acceptEdits, auto or \
             plan. Edit the automation's permission mode to run it again."
        );
        let saved = agent_execution::registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .automation(automation.id)
            .unwrap();
        assert!(!saved.enabled);
    }

    #[test]
    fn scoped_inspection_requires_authentication_instead_of_falling_back_to_user() {
        let root = tempfile::tempdir().unwrap();
        let registry =
            crate::host::model::Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("automation-scope-fixture"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        });
        state.install_model(registry);
        let host = Arc::new(crate::host::state::HostLock::new(state));
        let error = process_under_control(
            &host,
            Authority::session("unregistered-capability".into()),
            Request::List {
                after: None,
                limit: 1,
            },
        )
        .unwrap_err();
        assert!(matches!(&error, Error::PermissionDenied(_)));
        assert!(
            error
                .to_string()
                .contains("Invalid agent session authority")
        );
        assert!(!error.to_string().contains("unregistered-capability"));
    }
    #[test]
    fn automation_lifecycle_audit_scopes_live_hosted_reads_and_never_dispatches() {
        use crate::automations::{Request, Response, Schedule, SchedulePattern, SessionMode};
        use std::time::{Duration, Instant};
        let name = "host::automations::service::tests::automation_lifecycle_audit_scopes_live_hosted_reads_and_never_dispatches";
        if crate::host::agent_execution::tests::isolated(name).is_some() {
            return;
        }
        let (host, request, path) = crate::host::agent_execution::tests::fixture();
        let accepted = agent_execution::submit(&host, &Authority::default(), request).unwrap();
        let registry = agent_execution::registry(&host).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
            if run.state.is_terminal() {
                assert_eq!(run.state, crate::agent_execution::State::Succeeded);
                break;
            }
            assert!(Instant::now() < deadline, "fake provider did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
        let (authority, own, other) = {
            let mut registry = registry.lock().unwrap();
            let session = registry.agent(accepted.agent.id).unwrap().session.unwrap();
            let authority = registry.retained_agent_authority(session.id()).unwrap();
            let own_definition = Definition {
                name: "Own receipt".into(),
                prompt: "PRIVATE_AUDIT_PROMPT".into(),
                agent: accepted.agent.spec.clone(),
                schedule: Schedule {
                    pattern: SchedulePattern::Daily { hour: 0, minute: 0 },
                    timezone: "UTC".into(),
                },
                session_mode: SessionMode::Fresh,
                precheck: None,
            };
            let own = registry
                .create_automation(
                    own_definition.clone(),
                    100,
                    DirectoryIdentity {
                        device: 1,
                        inode: 1,
                    },
                )
                .unwrap();
            let other_path = path.parent().unwrap().join("other-project");
            std::fs::create_dir(&other_path).unwrap();
            registry
                .apply(&crate::workspace_model::ModelEdit::OpenProject {
                    repository: other_path.clone(),
                    name: "Other project".into(),
                    kind: crate::model::ProjectKind::Folder,
                })
                .unwrap();
            let mut other_definition = own_definition;
            other_definition.agent.target.repository = other_path.clone();
            other_definition.agent.target.directory = other_path;
            let other = registry
                .create_automation(
                    other_definition,
                    100,
                    DirectoryIdentity {
                        device: 1,
                        inode: 2,
                    },
                )
                .unwrap();
            registry
                .remove_automation(own.id, own.revision, 200)
                .unwrap();
            (authority, own.id, other.id)
        };
        let root = path.parent().unwrap();
        let prompts = std::fs::read(root.join("prompts")).unwrap();
        let spawns = std::fs::read(root.join("spawns")).unwrap();
        for _ in 0..3 {
            let Response::Audit(page) = process_under_control(
                &host,
                authority.clone(),
                Request::Audit {
                    id: own,
                    before: None,
                    limit: 100,
                },
            )
            .unwrap() else {
                panic!("expected audit");
            };
            assert_eq!(page.events.len(), 2);
            let invalid_cursor = crate::host::client::process_request(
                &host,
                1,
                crate::terminal_ipc::ClientRequest::Automation {
                    authority: authority.clone(),
                    operation: Request::Audit {
                        id: own,
                        before: Some(u64::MAX),
                        limit: 100,
                    },
                },
            )
            .unwrap_err();
            assert_eq!(
                invalid_cursor.code,
                crate::terminal_ipc::ProtocolErrorCode::InvalidRequest
            );
            assert!(
                !serde_json::to_string(&page)
                    .unwrap()
                    .contains("PRIVATE_AUDIT_PROMPT")
            );
            let error = process_under_control(
                &host,
                authority.clone(),
                Request::Audit {
                    id: other,
                    before: None,
                    limit: 100,
                },
            )
            .unwrap_err();
            assert!(matches!(&error, Error::PermissionDenied(_)));
            assert!(
                error.to_string().contains("only in their own project"),
                "{error}"
            );
            // Exercise the real dispatcher classification as well as the
            // service's project-scoped typed failure, without another actor.
            let error = crate::host::client::process_request(
                &host,
                1,
                crate::terminal_ipc::ClientRequest::Automation {
                    authority: authority.clone(),
                    operation: Request::Audit {
                        id: other,
                        before: None,
                        limit: 100,
                    },
                },
            )
            .unwrap_err();
            assert_eq!(
                error.code,
                crate::terminal_ipc::ProtocolErrorCode::PermissionDenied
            );
        }
        assert_eq!(std::fs::read(root.join("prompts")).unwrap(), prompts);
        assert_eq!(std::fs::read(root.join("spawns")).unwrap(), spawns);
        assert!(matches!(
            agent_execution::process(
                &host,
                Authority::default(),
                crate::agent_execution::Operation::Close(accepted.agent.id)
            )
            .unwrap(),
            crate::agent_execution::Reply::Closed(_)
        ));
        assert!(
            process_under_control(
                &host,
                authority,
                Request::Audit {
                    id: own,
                    before: None,
                    limit: 1
                }
            )
            .is_err(),
            "closed session cannot retain project read authority"
        );
    }

    /// A firing belongs to the project it ran in. Moving its automation
    /// carries neither earlier diagnostics into the destination project nor
    /// later ones back to the source.
    #[test]
    fn hosted_run_history_keeps_each_firing_in_the_project_it_ran_in() {
        use crate::automations::{
            Precheck, Request, Response, Schedule, SchedulePattern, SessionMode, SkipReason,
            Trigger,
        };
        use crate::host::automations::precheck::Outcome;
        use std::time::{Duration, Instant};
        let name = "host::automations::service::tests::hosted_run_history_keeps_each_firing_in_the_project_it_ran_in";
        if crate::host::agent_execution::tests::isolated(name).is_some() {
            return;
        }
        let (host, request, path) = crate::host::agent_execution::tests::fixture();
        let accepted = agent_execution::submit(&host, &Authority::default(), request).unwrap();
        let registry = agent_execution::registry(&host).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
            if run.state.is_terminal() {
                break;
            }
            assert!(Instant::now() < deadline, "fake provider did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
        let identity = || DirectoryIdentity {
            device: 1,
            inode: 1,
        };
        let (authority, arrived, departed, own_firings) = {
            let mut registry = registry.lock().unwrap();
            let session = registry.agent(accepted.agent.id).unwrap().session.unwrap();
            let authority = registry.retained_agent_authority(session.id()).unwrap();
            let other = path.parent().unwrap().join("other-project");
            std::fs::create_dir(&other).unwrap();
            registry
                .apply(&crate::workspace_model::ModelEdit::OpenProject {
                    repository: other.clone(),
                    name: "Other project".into(),
                    kind: crate::model::ProjectKind::Folder,
                })
                .unwrap();
            let own = Definition {
                name: "Moved".into(),
                prompt: "Check".into(),
                agent: accepted.agent.spec.clone(),
                schedule: Schedule {
                    pattern: SchedulePattern::Daily { hour: 0, minute: 0 },
                    timezone: "UTC".into(),
                },
                session_mode: SessionMode::Fresh,
                precheck: Some(Precheck {
                    command: "exit 1".into(),
                    timeout_seconds: 1,
                }),
            };
            let mut elsewhere = own.clone();
            elsewhere.agent.target.repository = other.clone();
            elsewhere.agent.target.directory = other;
            elsewhere.agent.target.placement = None;
            let mut now = 100;
            let mut fire = |registry: &mut crate::host::model::Registry,
                            automation: &crate::automations::Automation,
                            diagnostic: &str| {
                now += 100;
                let claim = registry
                    .claim_automation(
                        automation.id,
                        automation.execution_revision,
                        None,
                        Trigger::Manual {
                            request: uuid::Uuid::new_v4(),
                        },
                        None,
                        true,
                        now,
                    )
                    .unwrap();
                registry
                    .finish_automation_precheck(
                        claim.firing.id,
                        Outcome::Stopped {
                            state: crate::automations::FiringState::Skipped,
                            skip: Some(SkipReason::PrecheckExit),
                            diagnostic: diagnostic.into(),
                        },
                        now + 1,
                    )
                    .unwrap();
                claim.firing.id
            };
            // Ran elsewhere, then moved into this conversation's project.
            let arrived = registry
                .create_automation(elsewhere.clone(), 100, identity())
                .unwrap();
            fire(&mut registry, &arrived, "OTHER_PROJECT_STDERR");
            let arrived = registry
                .edit_automation(arrived.id, arrived.revision, own.clone(), 150, identity())
                .unwrap();
            let arrived_own = fire(&mut registry, &arrived, "own after arrival");
            // Ran here, then moved away.
            let departed = registry.create_automation(own, 100, identity()).unwrap();
            let departed_own = fire(&mut registry, &departed, "own before departure");
            let departed = registry
                .edit_automation(departed.id, departed.revision, elsewhere, 150, identity())
                .unwrap();
            fire(&mut registry, &departed, "OTHER_PROJECT_STDERR");
            assert_eq!(
                registry
                    .automation_history(Some(departed.id), None, 100)
                    .unwrap()
                    .firings
                    .len(),
                2,
                "management history keeps every firing"
            );
            (
                authority,
                arrived.id,
                departed.id,
                [arrived_own, departed_own],
            )
        };
        for (id, own_firing) in [arrived, departed].into_iter().zip(own_firings) {
            let Response::History(page) = process_under_control(
                &host,
                authority.clone(),
                Request::History {
                    id: Some(id),
                    before: None,
                    limit: 1,
                },
            )
            .unwrap() else {
                panic!("expected history");
            };
            assert_eq!(
                page.firings
                    .iter()
                    .map(|firing| firing.id)
                    .collect::<Vec<_>>(),
                [own_firing]
            );
            assert_eq!(page.next, None, "the page boundary ignores other projects");
            assert!(
                !serde_json::to_string(&page)
                    .unwrap()
                    .contains("OTHER_PROJECT_STDERR")
            );
        }
        assert!(
            process_under_control(
                &host,
                authority,
                Request::History {
                    id: Some(uuid::Uuid::new_v4()),
                    before: None,
                    limit: 1,
                },
            )
            .is_err_and(|error| matches!(error, Error::PermissionDenied(_)))
        );
    }

    /// A live hosted conversation's request for approval is told to every
    /// window watching notifications, which offer it without polling.
    #[test]
    fn a_hosted_proposal_is_told_to_windows_watching_notifications() {
        use crate::automations::{Response, Schedule, SchedulePattern, SessionMode};
        use std::time::{Duration, Instant};
        let name = "host::automations::service::tests::a_hosted_proposal_is_told_to_windows_watching_notifications";
        if crate::host::agent_execution::tests::isolated(name).is_some() {
            return;
        }
        let (host, request, _) = crate::host::agent_execution::tests::fixture();
        let accepted = agent_execution::submit(&host, &Authority::default(), request).unwrap();
        let registry = agent_execution::registry(&host).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
            if run.state.is_terminal() {
                assert_eq!(run.state, crate::agent_execution::State::Succeeded);
                break;
            }
            assert!(Instant::now() < deadline, "fake provider did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
        let authority = {
            let registry = registry.lock().unwrap();
            let session = registry.agent(accepted.agent.id).unwrap().session.unwrap();
            registry.retained_agent_authority(session.id()).unwrap()
        };
        let (watching, unwatched) = {
            let mut host = host.lock().unwrap();
            let watching = host.register_client(1);
            let unwatched = host.register_client(2);
            host.watch_notifications(
                1,
                crate::notifications::Presence {
                    window: "primary".into(),
                    application: Default::default(),
                    focused_target: None,
                    sessions: Vec::new(),
                    focused: None,
                },
            );
            (watching, unwatched)
        };
        // Everything sent so far, without waiting for more.
        let told = |delivery: &crate::host::delivery::DeliveryQueue| {
            delivery.close_after_drain();
            std::iter::from_fn(|| delivery.pop())
                .filter_map(|outbound| match outbound.message {
                    crate::terminal_ipc::ServerMessage::AutomationProposalsChanged {
                        repository,
                    } => Some(repository),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let Response::Proposal(proposal) = process_under_control(
            &host,
            authority,
            Request::Propose {
                request: Uuid::new_v4(),
                definition: Definition {
                    name: "Nightly recap".into(),
                    prompt: "Summarize the day".into(),
                    agent: accepted.agent.spec.clone(),
                    schedule: Schedule {
                        pattern: SchedulePattern::Daily { hour: 0, minute: 0 },
                        timezone: "UTC".into(),
                    },
                    session_mode: SessionMode::Fresh,
                    precheck: None,
                },
            },
        )
        .unwrap() else {
            panic!("expected a proposal");
        };
        assert_eq!(
            told(&watching),
            [proposal.definition.agent.target.repository.clone()]
        );
        assert!(told(&unwatched).is_empty());
        assert!(matches!(
            agent_execution::process(
                &host,
                Authority::default(),
                crate::agent_execution::Operation::Close(accepted.agent.id)
            )
            .unwrap(),
            crate::agent_execution::Reply::Closed(_)
        ));
    }
}
