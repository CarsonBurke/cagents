use super::*;
use crate::update::impact::{DaemonImpact, Disposition, Policy, SessionImpact, SessionKind};
use sha2::{Digest, Sha256};

/// The caller holds the control gate. SQL and actor barriers deliberately run
/// without HostState: both may need host events to finish admitted work.
pub(in crate::host) fn inspect(
    host: &SharedHost,
    policy: Policy,
    fence_actors: bool,
) -> Result<DaemonImpact, ProtocolError> {
    let (registry, handles) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.execution_registry.clone(),
            state
                .chats
                .iter()
                .filter(|(_, record)| record.handle.owns_work())
                .map(|(&id, record)| (id, record.handle.clone()))
                .collect::<Vec<_>>(),
        )
    };
    let mut blockers = Vec::new();
    if let Some(registry) = registry {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        blockers.extend(registry.agent_update_blockers().map_err(upgrade_error)?);
        blockers.extend(
            registry
                .automation_update_blockers()
                .map_err(upgrade_error)?,
        );
    }
    if fence_actors {
        for (id, handle) in handles {
            match handle.check_update_readiness().map_err(upgrade_error)? {
                crate::chat::runtime::UpdateReadiness::Unready => blockers.push(format!(
                    "Conversation {id} is still processing accepted provider work; retry after it settles."
                )),
                crate::chat::runtime::UpdateReadiness::Busy
                    if policy == Policy::WhenIdle
                        && !matches!(handle.native_continuity(), crate::chat::NativeContinuity::Durable { .. }) => {
                    blockers.push(format!("Conversation {id} has accepted work; wait for it to finish."));
                }
                _ => {}
            }
        }
    }
    let mut impact = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_impact(policy)?;
    impact.blockers.extend(blockers);
    Ok(impact)
}

impl HostState {
    pub(crate) fn upgrade_impact(&mut self, policy: Policy) -> Result<DaemonImpact, ProtocolError> {
        let capability = self
            .shutdown_preflight(Instant::now())
            .map_err(|outcome| upgrade_error(format!("Cannot inspect update impact: {outcome:?}")))?
            .capability;
        let mut impact = DaemonImpact {
            capability,
            policy,
            sessions: Vec::new(),
            blockers: Vec::new(),
        };
        let holders = holder_directory()
            .ok()
            .filter(|directory| crate::secure_file::prepare_private_directory(directory).is_ok());
        let can_carry = |id| {
            holders
                .as_ref()
                .is_some_and(|directory| holder_socket_fits(&holder_socket(directory, id)))
        };
        if !self.native_closing.is_empty() {
            impact
                .blockers
                .push("Wait for native provider cleanup to finish.".into());
        }
        if !self.spawning.is_empty() || !self.chat_spawning.is_empty() {
            impact
                .blockers
                .push("Wait for starting sessions to finish opening.".into());
        }
        if self
            .chat_handoffs
            .values()
            .any(|receipt| !receipt.unacknowledgeable(Instant::now()))
        {
            impact
                .blockers
                .push("Finish saving the pending return to terminal.".into());
        }
        for (&id, record) in &self.terminals {
            if record.handle.is_completed() || self.chats.contains_key(&id) {
                continue;
            }
            let source = self.descriptor(0, id)?.session;
            let (disposition, reason) = if record.execution_environment.is_none()
                && record.handle.holder_identity().is_some()
                && can_carry(id)
            {
                (
                    Disposition::Continues,
                    "The terminal and its running processes stay alive.",
                )
            } else {
                (
                    Disposition::Blocked,
                    "This terminal cannot preserve its live process through this update; finish it or close it before updating.",
                )
            };
            impact.sessions.push(SessionImpact {
                id,
                kind: SessionKind::Terminal,
                title: display_title(
                    record
                        .conversation_title
                        .as_ref()
                        .map(|(_, title)| title.as_str())
                        .unwrap_or(&source.launch.program),
                    &source.launch.working_directory,
                ),
                identity: identity(&(self.inventory_generation, &source))?,
                disposition,
                reason: reason.into(),
            });
        }
        for (&id, record) in &self.chats {
            if !record.handle.owns_work() {
                continue;
            }
            let (busy, ready) = record.handle.update_activity();
            let confirmed = record.handle.confirmed_session_id();
            let mut launch = record.session.launch.clone();
            launch.session_id = confirmed.clone();
            let aligned = crate::chat::protocol::align_resume_identity(&mut launch).is_ok();
            let durable = matches!(
                record.handle.native_continuity(),
                crate::chat::NativeContinuity::Durable { .. }
            );
            let hosted_source = !super::super::chat::owns_codex_listener(&record.session)
                && self
                    .terminals
                    .get(&id)
                    .is_some_and(|terminal| !terminal.handle.is_completed());
            let environment_ready = super::super::chat::validate_upgrade_environment(
                &launch,
                hosted_source,
                &record.handle,
            )
            .is_ok();
            let (disposition, reason) = if record.returning_terminal {
                (
                    Disposition::Blocked,
                    "Finish the pending return to terminal or close this conversation.",
                )
            } else if record.handle.is_completed() {
                (
                    Disposition::Blocked,
                    "Close this conversation to finish its pending provider cleanup, then retry.",
                )
            } else if !ready || !aligned || !environment_ready || launch.terminal_recipe().is_none()
            {
                (
                    Disposition::Blocked,
                    "Wait for the provider to confirm its resumable conversation, or close it.",
                )
            } else if durable
                && self.terminals.get(&id).is_some_and(|terminal| {
                    !terminal.handle.is_completed()
                        && (!can_carry(id) || terminal.handle.holder_identity().is_none())
                })
            {
                (
                    Disposition::Blocked,
                    "The paired console cannot preserve its live process through this update.",
                )
            } else if durable {
                (
                    Disposition::Continues,
                    "The provider, active turn, and pending approvals stay alive while the application reconnects.",
                )
            } else if busy && policy == Policy::WhenIdle {
                (
                    Disposition::WaitsForIdle,
                    "The update will wait for this conversation to become idle, then resume it.",
                )
            } else if busy {
                (
                    Disposition::Interrupted,
                    "The active turn will be interrupted; the saved conversation will resume.",
                )
            } else {
                (
                    Disposition::Resumes,
                    "The saved conversation will reopen with a new provider process.",
                )
            };
            impact.sessions.push(SessionImpact {
                id,
                kind: SessionKind::NativeChat,
                title: display_title(
                    record
                        .handle
                        .update_title()
                        .as_deref()
                        .unwrap_or(record.session.launch.provider.label()),
                    &record.session.launch.recipe.working_directory,
                ),
                identity: identity(&(
                    self.inventory_generation,
                    &record.session,
                    confirmed,
                    record.handle.native_continuity(),
                ))?,
                disposition,
                reason: reason.into(),
            });
        }
        impact
            .sessions
            .sort_by_key(|session| (session.kind, session.id));
        Ok(impact)
    }
}

fn display_title(name: &str, directory: &std::path::Path) -> String {
    let clean = |text: &str| {
        text.chars()
            .filter(|character| !character.is_control())
            .take(80)
            .collect::<String>()
    };
    let directory = directory.file_name().unwrap_or_default().to_string_lossy();
    format!("{} · {}", clean(name), clean(&directory))
}

fn identity(value: &impl serde::Serialize) -> Result<String, ProtocolError> {
    let bytes = serde_json::to_vec(value).map_err(|error| upgrade_error(error.to_string()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
