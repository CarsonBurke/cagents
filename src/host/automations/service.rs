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
    if authority.token().is_some() && !request.allowed_from_hosted_conversation() {
        return Err("An agent's child-execution capability does not grant automation management. Prepare a proposal for explicit user approval.".into());
    }
    let registry = agent_execution::registry(host)?;
    if authority.token().is_some() {
        return process_hosted(host, &authority, request);
    }
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
        Request::AllPendingProposals {
            identity,
            sequence,
            after,
            limit,
        } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pending_automation_proposals_page(identity, sequence, after, limit)
                .map(Response::ProposalPage);
        }
        Request::ProposalDelivery => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_proposal_delivery()
                .map(Response::ProposalDelivery);
        }
        Request::ReviewProposal { identity, id } => {
            return registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .review_automation_proposal(identity, id)
                .map(Response::ProposalReview);
        }
        Request::ResolveReviewedProposal {
            identity,
            proposal,
            approved,
        } => {
            let current = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .review_automation_proposal(identity, proposal.id)?;
            if current.proposal.revision != proposal.revision {
                return Err(
                    "The automation proposal changed; review its current definition".into(),
                );
            }
            if current.proposal.state != crate::automations::ProposalState::Pending {
                return Ok(Response::ProposalReview(current));
            }
            if current.proposal != proposal {
                return Err("The automation proposal changed; review it again".into());
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
                .resolve_automation_proposal_in_registry(identity, &proposal, next, now()?)?;
            super::proposal_delivery::publish(host, &registry);
            Response::ProposalReview(crate::automations::ProposalReview {
                identity,
                proposal: Box::new(resolved),
            })
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
            super::proposal_delivery::publish(host, &registry);
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

/// Inspection and proposal ownership follow authenticated session lineage.
/// A plain token, stale session or delegated child never gains scheduler
/// mutation authority through this path.
fn process_hosted(
    host: &SharedHost,
    authority: &crate::agent_execution::Authority,
    request: crate::automations::Request,
) -> Result<crate::automations::Response, String> {
    use crate::automations::{Request, Response};
    let (owner, source) = agent_execution::caller(host, authority)?;
    let source = source.ok_or("A live hosted conversation is required")?;
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
                .ok_or("This conversation has no such automation proposal")?;
            return Ok(Response::Proposal(Box::new(proposal)));
        }
        Request::Propose {
            request,
            definition,
        } => {
            let previous = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_proposal(*request, Some(&owner))?;
            if let Some(previous) = previous {
                if previous.requested != *definition {
                    return Err(
                        "This request ID belongs to another saved definition; use a new request ID"
                            .into(),
                    );
                }
                // Exact receipt retry also retries the best-effort model hint.
                // It never inserts a proposal or changes consent. Wake only
                // the existing coordinator so durable attention can advance.
                super::proposal_delivery::publish(host, &registry);
                super::wake(host);
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
                .ok_or("The requesting session is no longer live")?
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
            Err("Hosted conversations can inspect and propose automations only in their own project".to_owned())
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
                Ok::<_, String>(channel.title.clone())
            }).transpose()?;
            let proposal = registry.lock().unwrap_or_else(|p| p.into_inner())
                .propose_automation(request, owner, requested, definition, identity, channel_title, now()?)?;
            super::proposal_delivery::publish(host, &registry);
            super::wake(host);
            Ok(Response::Proposal(Box::new(proposal)))
        }
        Request::List { after, limit } => registry.lock().unwrap_or_else(|p| p.into_inner())
            .project_automations(&repository, after, limit).map(Response::Listed),
        Request::Show { id } => {
            let automation = registry.lock().unwrap_or_else(|p| p.into_inner()).automation(id)?;
            require_scope(&automation.definition.agent.target.repository)?;
            Ok(Response::Automation(automation))
        }
        Request::History { id: Some(id), before, limit } => {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            require_scope(&registry.automation(id)?.definition.agent.target.repository)?;
            registry.automation_history(Some(id), before, limit).map(Response::History)
        }
        Request::Audit { id, before, limit } => {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            registry.project_automation_audit(&repository, id, before, limit).map(Response::Audit)
        }
        Request::ShowFiring { id } => {
            let detail = registry.lock().unwrap_or_else(|p| p.into_inner()).automation_run_detail(id)?;
            require_scope(&detail.definition.agent.target.repository)?;
            Ok(Response::Detail(detail))
        }
        Request::ShowRun { run } => {
            let detail = registry.lock().unwrap_or_else(|p| p.into_inner()).automation_detail_for_run(run)?;
            require_scope(&detail.definition.agent.target.repository)?;
            Ok(Response::Detail(detail))
        }
        Request::History { id: None, .. } => Err("Choose an automation ID for scoped run history".into()),
        _ => Err("An agent's child-execution capability does not grant automation management. Prepare a proposal for explicit user approval.".into()),
    }
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
        assert!(error.contains("does not grant automation management"));
        assert!(!error.contains("private-child-capability"));
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
        assert!(error.contains("Invalid agent session authority"));
        assert!(!error.contains("unregistered-capability"));
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
            assert!(error.contains("only in their own project"), "{error}");
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
}
