//! Explicit close consent joins current presentation CAS to exact cleanup.
use super::*;
use crate::{
    agent_execution::{HostedSession, PresentationCloseObservation, PresentationCloseOwner, PresentationCloseTarget},
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
    let (proof, actor_instance, pending) = {
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
        (state.proof().clone(), instance, state.native_closing.get(&source.id).cloned())
    };
    let receipt = chat::native_holder::closed_source(source, &proof)?;
    let owner = pending.as_ref().map(|pending| pending.owner_record().clone())
        .or_else(|| receipt.as_ref().map(|(record, _)| record.clone()))
        .map(Ok).unwrap_or_else(|| registry::evidence(
            &registry::socket(&registry::directory(Kind::Native)?, source.id), Kind::Native)
            .and_then(|record| record.ok_or_else(|| "No authenticated current native owner is available".to_owned())))?;
    let paired_owner = if let Some((closed, _)) = &receipt {
        if closed != &owner { return Err("The current native receipt belongs to a different owner".into()); }
        pending.as_ref().and_then(|pending| pending.paired_owner()).cloned()
            .or(chat::native_holder::closed_pair_holder(source, &proof)?)
    } else {
        let (current, previous, pair) = if Handle::is_remote_record(&owner) {
            let inspection = Handle::inspect_remote(&owner, proof.clone())?;
            (inspection.source, inspection.previous_sources, inspection.paired_holder)
        } else {
            let recovery = chat::native_holder::inspect(&owner, &proof)?.0;
            (recovery.source, recovery.previous_sources, recovery.paired_holder)
        };
        if current != *source && !previous.contains(source) {
            return Err("The current holder does not recognize this exact retained provider source".into());
        }
        pair
    };
    let id = source.id;
    let result = PresentationCloseTarget {observation, host: proof.clone(), owner: owner_identity(&owner),
        actor_instance, paired_owner: paired_owner.as_ref().map(owner_identity)};
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
