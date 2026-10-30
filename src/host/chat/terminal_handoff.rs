//! Issued terminal-retirement evidence survives a window's durable ACK.
//! This module observes the existing exact receipt; it never stops a process.
use super::*;
use crate::daemon::holder_registry::{OriginalRole, Record as HolderRecord};

#[derive(Clone)]
pub(super) struct IssuedRetirement {
    instance: uuid::Uuid,
    proof: HostSessionProof,
    holder: HolderRecord,
    role: Option<OriginalRole>,
}

impl IssuedRetirement {
    pub(super) fn capture(
        worker: &crate::terminal::TerminalHandle,
        terminal: &crate::model::TerminalSession,
        proof: &HostSessionProof,
    ) -> Result<Self, ProtocolError> {
        let (holder, role) = worker.holder_retirement_identity().ok_or_else(|| {
            unavailable(
                "The replacement has no authenticated original holder; handoff remains unpublished",
            )
        })?;
        if holder.terminal != terminal.id {
            return Err(unavailable(
                "The replacement holder identity changed before publication",
            ));
        }
        Ok(Self {
            instance: uuid::Uuid::new_v4(),
            proof: proof.clone(),
            holder,
            role,
        })
    }
}

/// An attachment may inspect the handoff while T is still owned. Only later
/// close/replay predicates decide whether that actual T owner permits action.
pub(super) fn require_binding(
    state: &HostState,
    chat_id: TerminalId,
    client: Option<ClientId>,
) -> Result<&TerminalHandoff, ProtocolError> {
    let receipt = state
        .chat_handoffs
        .get(&chat_id)
        .ok_or_else(|| unavailable("The issued terminal handoff is no longer available"))?;
    let issued = receipt
        .retirement
        .as_ref()
        .ok_or_else(|| unavailable("This handoff has no retained original retirement identity"))?;
    validate_proof(Some(&issued.proof), state.proof())?;
    validate_proof(receipt.source.proof.as_ref(), state.proof())?;
    if state.shutting_down
        || receipt.source.id != chat_id
        || state.chats.contains_key(&chat_id)
        || state.chat_spawning.contains_key(&chat_id)
        || state.terminal_is_starting(chat_id)
        || state.terminals.contains_key(&chat_id)
        || state.native_closing.contains_key(&chat_id)
        || state.native_actor_startups.contains_key(&chat_id)
    {
        return Err(unavailable(
            "The original chat identity changed during terminal recovery",
        ));
    }
    if let Some(client) = client {
        receipt.require_attached(client)?;
    }
    Ok(receipt)
}

fn terminal_transition(state: &HostState, terminal: TerminalId) -> bool {
    state.terminal_is_starting(terminal)
        || state.chat_spawning.contains_key(&terminal)
        || state.chats.contains_key(&terminal)
        || state.native_closing.contains_key(&terminal)
        || state.native_actor_startups.contains_key(&terminal)
}

pub(super) fn require_live_replay(
    state: &HostState,
    chat_id: TerminalId,
    client: ClientId,
) -> Result<(), ProtocolError> {
    let receipt = require_binding(state, chat_id, Some(client))?;
    let issued = receipt
        .retirement
        .as_ref()
        .expect("validated private binding");
    if receipt.terminal_retired
        || terminal_transition(state, receipt.terminal.id)
        || !state
            .terminals
            .get(&receipt.terminal.id)
            .is_some_and(|record| {
                !record.handle.is_completed()
                    && record.handle.holder_identity() == Some(&issued.holder)
                    && matches_paired_terminal(&record.session, &receipt.terminal, &issued.proof)
            })
    {
        return Err(unavailable(
            "The issued terminal cannot be replayed as a live replacement; its exact retirement must be confirmed",
        ));
    }
    Ok(())
}

fn require_close_owner(state: &HostState, receipt: &TerminalHandoff) -> Result<(), ProtocolError> {
    let issued = receipt
        .retirement
        .as_ref()
        .expect("validated private binding");
    if terminal_transition(state, receipt.terminal.id)
        || state.any_live(&[receipt.terminal.id])
        || state
            .terminals
            .get(&receipt.terminal.id)
            .is_some_and(|record| {
                !record.handle.is_completed()
                    || record.handle.holder_identity() != Some(&issued.holder)
                    || !matches_paired_terminal(&record.session, &receipt.terminal, &issued.proof)
            })
    {
        return Err(unavailable(
            "The issued terminal still has an owner or its ownership changed; original chat recovery is retained",
        ));
    }
    Ok(())
}

pub(super) fn close(
    host: &SharedHost,
    client: ClientId,
    chat_id: TerminalId,
) -> Result<Option<ServerResponse>, ProtocolError> {
    let (issued, source, terminal) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.chat_handoffs.contains_key(&chat_id) {
            return Ok(None);
        }
        let receipt = require_binding(&state, chat_id, Some(client))?;
        require_close_owner(&state, receipt)?;
        if receipt.terminal_retired {
            return Ok(Some(ServerResponse::ChatClosed {
                chat_id,
                snapshot: receipt.snapshot(),
            }));
        }
        (
            receipt
                .retirement
                .clone()
                .expect("validated private binding"),
            receipt.source.clone(),
            receipt.terminal.clone(),
        )
    };
    // File/lease/process observations stay outside HostState. Absence alone
    // cannot produce true, and this observer has no process-signal operation.
    let confirmed = crate::terminal::holder::cleanup::confirmed_with_role(
        &terminal,
        &issued.holder,
        &issued.proof,
        issued.role.as_ref(),
    )
    .map_err(unavailable)?;
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let receipt = require_binding(&state, chat_id, Some(client))?;
    if receipt.source != source
        || receipt.terminal != terminal
        || receipt
            .retirement
            .as_ref()
            .is_none_or(|current| current.instance != issued.instance)
    {
        return Err(unavailable(
            "The exact issued handoff changed while observing retirement",
        ));
    }
    require_close_owner(&state, receipt)?;
    if !confirmed {
        return Err(unavailable(
            "The original terminal has not positively confirmed retirement; original chat recovery is retained",
        ));
    }
    let receipt = state
        .chat_handoffs
        .get_mut(&chat_id)
        .expect("revalidated exact handoff");
    let newly_retired = !receipt.terminal_retired;
    if newly_retired {
        receipt.terminal_retired = true;
        receipt.awaiting_ack = false;
        receipt.generation += 1;
    }
    let snapshot = receipt.snapshot();
    if newly_retired {
        state.inventory_generation += 1;
    }
    Ok(Some(ServerResponse::ChatClosed { chat_id, snapshot }))
}
