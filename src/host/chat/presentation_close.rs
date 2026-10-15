//! Explicit close consent joins current presentation CAS to exact cleanup.
use super::*;
use crate::{
    agent_execution::{HostedSession, PresentationCloseBackend, PresentationCloseObservation, PresentationCloseOwner, PresentationCloseTarget},
    daemon::holder_registry::{self as registry, Kind},
};

pub(in crate::host) fn owner_identity(record: &registry::Record) -> PresentationCloseOwner {
    PresentationCloseOwner {version: record.version, native: record.kind == Kind::Native,
        terminal: record.terminal, application: record.application.clone(), boot: record.boot.clone(),
        holder_pid: record.holder.pid, holder_start: record.holder.start,
        provider_pid: record.root.pid, provider_start: record.root.start}
}

/// Inspection never sends Close, Release, adoption, or a provider command.
/// Closed receipts remain usable after a late durable CAS refusal.
pub(in crate::host) fn capture(host: &SharedHost, observation: PresentationCloseObservation)
    -> Result<PresentationCloseTarget, String> {
    let HostedSession::Chat(source) = &observation.source else {
        return Err("Combined close requires an exact native provider source".into());
    };
    let (proof, actor_instance, hosted_handle, pending) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down || state.chat_handoffs.contains_key(&source.id) {
            return Err("Provider ownership is transitioning; inspect again after it settles".into());
        }
        let instance = state.chats.get(&source.id).map(|record| {
            if !HostedSession::Chat(record.session.clone()).same_process(&observation.source)
                || record.execution_instance.is_nil() {
                return Err("The current hosted provider no longer matches this agent".to_owned());
            }
            Ok(record.execution_instance)
        }).transpose()?;
        (state.proof().clone(), instance, state.chats.get(&source.id).map(|record| record.handle.clone()),
            state.native_closing.get(&source.id).cloned())
    };
    let receipt = chat::native_holder::closed_source(source, &proof)?;
    let owner = pending.as_ref().map(|pending| pending.owner_record().clone())
        .or_else(|| receipt.as_ref().map(|(record, _)| record.clone()))
        .map(Ok).unwrap_or_else(|| registry::evidence(
            &registry::socket(&registry::directory(Kind::Native)?, source.id), Kind::Native)
            .and_then(|record| record.ok_or_else(|| "No authenticated current native owner is available".to_owned())))?;
    let mut backend = pending.as_ref().and_then(|pending| pending.consented_backend()).cloned();
    // Refresh the actual backend, never the immutable helper's original root.
    // The caller holds this chat's transition gate throughout capture/close.
    if let Some(handle) = &hosted_handle {
        if receipt.is_none() || owner.holder.is_current() {
            handle.refresh_retained_state()?;
            let current = handle.provider_process().ok_or("The hosted provider incarnation is unknown; no close consent was issued")?;
            if backend.as_ref().is_some_and(|captured| captured != &current) {
                return Err("The retained close owner belongs to a previous backend incarnation; its successor was preserved".into());
            }
            backend = Some(current);
        } else if backend.is_none() {
            // This cache belongs to the retained exact handle and its final
            // authenticated publication; never refresh a new handle by ID.
            backend = handle.provider_process();
        }
    }
    if receipt.is_some() && owner.holder.is_current() && Handle::is_remote_record(&owner) {
        let inspection = Handle::inspect_remote(&owner, proof.clone())?;
        let current = inspection.state.provider_process.ok_or("The closing helper has no verified backend incarnation")?;
        if !inspection.state.completed {
            return Err("The receipt owner now has an active backend; no earlier consent authorizes it".into());
        }
        if backend.as_ref().is_some_and(|captured| captured != &current) {
            return Err("The backend changed after close consent; its replacement was preserved".into());
        }
        backend = Some(current);
    }
    let paired_owner = if let Some((closed, _)) = &receipt {
        if closed != &owner { return Err("The current native receipt belongs to a different owner".into()); }
        pending.as_ref().and_then(|pending| pending.paired_owner()).cloned()
            .or(chat::native_holder::closed_pair_holder(source, &proof)?)
    } else {
        let (current, previous, pair, actual_backend) = if Handle::is_remote_record(&owner) {
            let inspection = Handle::inspect_remote(&owner, proof.clone())?;
            (inspection.source, inspection.previous_sources, inspection.paired_holder, inspection.state.provider_process)
        } else {
            let recovery = chat::native_holder::inspect(&owner, &proof)?.0;
            (recovery.source, recovery.previous_sources, recovery.paired_holder, recovery.server_process)
        };
        let actual_backend = actual_backend.ok_or("The current native owner has no authenticated backend incarnation")?;
        if backend.as_ref().is_some_and(|old| old != &actual_backend) {
            return Err("The provider backend changed during inspection; inspect its new incarnation again".into());
        }
        backend = Some(actual_backend);
        if current != *source && !previous.contains(source) {
            return Err("The current holder does not recognize this exact retained provider source".into());
        }
        pair
    };
    let backend = match backend {
        Some(process) => PresentationCloseBackend::Process {pid: process.pid, start: process.start},
        None if receipt.as_ref().is_some_and(|(record, paired)| *paired && record == &owner)
            && !owner.holder.is_current() => {
                if Handle::is_remote_record(&owner) {
                    let saved = chat::native_holder::closed_source_proof(source, &proof)?
                        .ok_or("The exact native cleanup proof is unavailable")?;
                    if !Handle::retained_retirement_proved(&owner, &saved)? {
                        return Err("The exact native helper has not proved final semantic retirement; its agent was retained".into());
                    }
                }
                PresentationCloseBackend::RetiredOwner
            },
        None => return Err("The exact provider incarnation is unknown; no teardown consent was issued".into()),
    };
    let id = source.id;
    let result = PresentationCloseTarget {observation, host: proof.clone(), owner: owner_identity(&owner),
        actor_instance, backend, paired_owner: paired_owner.as_ref().map(owner_identity)};
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.proof() != &proof || state.chats.get(&id).map(|record| record.execution_instance) != actor_instance {
        return Err("Provider ownership changed during inspection; inspect again".into());
    }
    Ok(result)
}

pub(super) fn require_current(host: &SharedHost, target: &PresentationCloseTarget) -> Result<(), String> {
    let fresh = capture(host, target.observation.clone())?;
    if fresh != *target {
        return Err("The exact provider or paired owner changed; no close consent was applied".into());
    }
    Ok(())
}

pub(in crate::host) fn transition_gate(host: &SharedHost, id: TerminalId) -> Arc<std::sync::Mutex<()>> {
    super::restart::transition_gate(host, id)
}
