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
    // A refused close can leave an authenticated cleanup-only attachment.
    // Reinstall its actual running owner before previewing checkpointability;
    // never waive a pending handoff or pretend a failed close completed.
    let cleanup_failures = super::super::chat::retained_actor::recover_cleanup_attachments(host);
    let (registry, handles, migrations) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.execution_registry.clone(),
            state
                .chats
                .iter()
                // An actor that already stopped admits nothing left to fence.
                .filter(|(_, record)| record.handle.owns_work() && !record.handle.is_completed())
                .map(|(&id, record)| (id, record.handle.clone()))
                .collect::<Vec<_>>(),
            state
                .chats
                .iter()
                .filter_map(|(id, record)| {
                    if !record.handle.owns_work() || record.handle.is_completed() {
                        return None;
                    }
                    let mut source = record.session.clone();
                    source.native_continuity = record.handle.native_continuity();
                    if source.native_continuity.retains_provider() {
                        source.launch.session_id = record.handle.confirmed_session_id();
                    }
                    let mut launch = source.launch.clone();
                    launch.session_id = record.handle.confirmed_session_id();
                    crate::chat::protocol::align_resume_identity(&mut launch).ok()?;
                    crate::upgrade::handoff::validate_chat_source(&source, &launch).ok()?;
                    Some(crate::upgrade::handoff::Chat {
                        source,
                        launch,
                        previous_sources: state.upgrade_chats.get(id).cloned().unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>(),
        )
    };
    let mut blockers = cleanup_failures;
    if let Some(registry) = registry {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        blockers.extend(registry.agent_update_blockers().map_err(upgrade_error)?);
        blockers.extend(
            registry
                .automation_update_blockers()
                .map_err(upgrade_error)?,
        );
    }
    if let Some(store) = crate::upgrade::handoff::Store::existing().map_err(upgrade_error)?
        && let Err(reason) = store.preflight_recovered(&migrations)
    {
        blockers.push(format!(
            "Upgrade recovery evidence cannot be retained: {reason}"
        ));
    }
    if fence_actors {
        for (id, handle) in handles {
            let readiness = match handle.check_update_readiness() {
                Ok(readiness) => readiness,
                // It stopped after being collected; the impact below reports
                // what is left of it.
                Err(_) if handle.is_completed() => continue,
                Err(error) => return Err(upgrade_error(error)),
            };
            match readiness {
                crate::chat::runtime::UpdateReadiness::Unready => blockers.push(format!(
                    "Conversation {id} is still processing accepted provider work; retry after it settles."
                )),
                crate::chat::runtime::UpdateReadiness::Busy
                    if policy == Policy::WhenIdle
                        && !handle.native_continuity().retains_provider() => {
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
    /// Whether this conversation's provider stopped on its own and left
    /// nothing for an update to carry: no live process, no retained server
    /// and no cleanup still owed. Its failure stays on record so the window
    /// can show it, but the record is all that is left, and the window
    /// restarts the saved conversation the same way it does after any daemon
    /// restart. A durable owner is different: its server may still hold a
    /// turn, and only an explicit close can release it.
    pub(in crate::host) fn provider_ended(
        &self,
        id: TerminalId,
        record: &super::super::chat::Record,
    ) -> bool {
        let reconnects = |continuity: &crate::chat::NativeContinuity| {
            matches!(continuity, crate::chat::NativeContinuity::ReconnectOnly)
        };
        record.handle.is_completed()
            && !record.returning_terminal
            && !self.native_closing.contains_key(&id)
            && reconnects(&record.session.native_continuity)
            && reconnects(&record.handle.native_continuity())
            // A terminal still running under this conversation is checkpointed
            // from the chat's confirmed identity, which a stopped actor no
            // longer vouches for.
            && self
                .terminals
                .get(&id)
                .is_none_or(|terminal| terminal.handle.is_completed())
            && record
                .paired_terminal
                .as_ref()
                .is_none_or(|terminal| terminal.is_completed())
            && matches!(
                super::super::chat::has_native_owner(&record.session),
                Ok(false)
            )
    }

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
        // Holder transport resolves long addresses through a private directory
        // FD; the full UUID endpoint need not fit directly in sockaddr_un.
        let can_carry = holders.is_some();
        if !self.native_closing.is_empty() {
            impact
                .blockers
                .push("Wait for native provider cleanup to finish.".into());
        }
        if !self.spawning.is_empty()
            || !self.chat_spawning.is_empty()
            || !self.chat_preparing.is_empty()
        {
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
                && can_carry
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
                interrupted_subagents: Vec::new(),
            });
        }
        for (&id, record) in &self.chats {
            if !record.handle.owns_work() {
                continue;
            }
            let (busy, ready) = record.handle.update_activity();
            let interrupted_subagents = record.handle.interrupt_subagents();
            let confirmed = record.handle.confirmed_session_id();
            let mut launch = record.session.launch.clone();
            launch.session_id = confirmed.clone();
            let aligned = crate::chat::protocol::align_resume_identity(&mut launch).is_ok();
            let continuity = record.handle.native_continuity();
            let durable = continuity.retains_provider();
            let retained_claude = matches!(
                continuity,
                crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
            ) && record.handle.actor_identity().is_some();
            let hosted_source = !super::super::chat::owns_codex_listener(&record.session)
                && self
                    .terminals
                    .get(&id)
                    .is_some_and(|terminal| !terminal.handle.is_completed());
            let environment = super::super::chat::validate_upgrade_environment(
                &launch,
                hosted_source,
                &record.handle,
            );
            // Preview the same issued source that preparation checkpoints.
            // A lagging presentation capability is not the installed helper's
            // ownership; obtain that capability from this exact live handle.
            let mut source = record.session.clone();
            source.native_continuity = continuity;
            if source.native_continuity.retains_provider() {
                source.launch.session_id = confirmed.clone();
            }
            let migration = crate::upgrade::handoff::validate_chat_source(&source, &launch);
            let (disposition, reason) = if record.returning_terminal {
                (
                    Disposition::Blocked,
                    "Finish the pending return to terminal or close this conversation.",
                )
            } else if self.provider_ended(id, record) {
                (
                    Disposition::Resumes,
                    "Its provider already stopped. The saved conversation will reopen with a new provider process.",
                )
            } else if record.handle.is_completed() {
                (
                    Disposition::Blocked,
                    "Its provider stopped but may still hold unfinished work. Close this conversation to release it.",
                )
            } else if let Err(reason) = &migration {
                (Disposition::Blocked, reason.as_str())
            } else if let Err(reason) = &environment {
                (Disposition::Blocked, reason.as_str())
            } else if !ready {
                (
                    Disposition::Blocked,
                    "Its provider is not ready yet. This clears on its own once it finishes starting; close the conversation if it does not.",
                )
            } else if (!aligned || launch.terminal_recipe().is_none()) && !retained_claude {
                (
                    Disposition::Blocked,
                    "Its provider has not reported which conversation it is running, so it cannot be resumed after the update. Send it a message or close it.",
                )
            } else if durable
                && self.terminals.get(&id).is_some_and(|terminal| {
                    !terminal.handle.is_completed()
                        && (!can_carry || terminal.handle.holder_identity().is_none())
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
            let reason = if disposition == Disposition::Interrupted
                && !interrupted_subagents.is_empty()
            {
                format!(
                    "The active turn and {} running background subagent(s) will stop; background work will not resume automatically.",
                    interrupted_subagents.len()
                )
            } else {
                reason.into()
            };
            impact.sessions.push(SessionImpact {
                id,
                kind: SessionKind::NativeChat,
                title: display_title(
                    record
                        .handle
                        .title()
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
                reason,
                interrupted_subagents: if disposition == Disposition::Interrupted {
                    interrupted_subagents
                } else {
                    Vec::new()
                },
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
